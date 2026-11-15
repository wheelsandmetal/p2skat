//! HTTP layer: routing, static files, SSE streams and fault injection.

use std::convert::Infallible;
use std::future::poll_fn;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use hyper::body::{Body as HttpBody, Bytes, Frame, Incoming, SizeHint};
use hyper::header::{HeaderValue, CACHE_CONTROL, CONTENT_TYPE, HOST};
use hyper::{Method, Request, Response, StatusCode};
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch, Semaphore};

use crate::rooms::{Actor, ApiError, State, Step, StreamState, MAX_BODY, MAX_JOIN_BODY, MAX_RELAY_BODY, RETRY};
use crate::util;

/// Request bodies being read at once may hold at most this many bytes in total,
/// so many slow 1 MiB relay POSTs cannot exhaust a router's memory.
const BODY_BUDGET: usize = 8 << 20;

pub const CSP: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; connect-src 'self'; img-src 'self' data:; style-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

/// Returned to hyper to cut a connection without a response (fault injection).
#[derive(Debug)]
pub struct Abort;

impl std::fmt::Display for Abort {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("connection dropped")
    }
}

impl std::error::Error for Abort {}

pub enum Body {
    Full(Option<Bytes>),
    Stream(mpsc::Receiver<Result<Bytes, Abort>>),
}

impl HttpBody for Body {
    type Data = Bytes;
    type Error = Abort;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, Abort>>> {
        match self.get_mut() {
            Body::Full(bytes) => Poll::Ready(bytes.take().map(|bytes| Ok(Frame::data(bytes)))),
            Body::Stream(receiver) => receiver.poll_recv(cx).map(|item| item.map(|chunk| chunk.map(Frame::data))),
        }
    }

    fn is_end_stream(&self) -> bool {
        matches!(self, Body::Full(None))
    }

    fn size_hint(&self) -> SizeHint {
        match self {
            Body::Full(bytes) => SizeHint::with_exact(bytes.as_ref().map_or(0, |bytes| bytes.len() as u64)),
            Body::Stream(_) => SizeHint::default(),
        }
    }
}

#[derive(Default)]
pub struct Stats {
    pub requests: AtomicU64,
}

pub struct Shared {
    state: Mutex<State>,
    changed: watch::Sender<u64>,
    web_dir: PathBuf,
    allowed_host: Option<String>,
    body_budget: Semaphore,
    pub stats: Stats,
}

impl Shared {
    pub fn new(state: State, web_dir: PathBuf, allowed_host: Option<String>) -> Shared {
        Shared { state: Mutex::new(state), changed: watch::channel(0).0, web_dir, allowed_host, body_budget: Semaphore::new(BODY_BUDGET), stats: Stats::default() }
    }

    pub fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Wake every stream and delayed response to re-check the state.
    pub fn notify(&self) {
        self.changed.send_modify(|version| *version = version.wrapping_add(1));
    }
}

fn response(status: u16, content_type: &str, cache: &str, body: Body) -> Response<Body> {
    let mut response = Response::new(body);
    *response.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_str(content_type).expect("valid content type"));
    headers.insert(CACHE_CONTROL, HeaderValue::from_str(cache).expect("valid cache header"));
    headers.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert("content-security-policy", HeaderValue::from_static(CSP));
    response
}

fn json_response(status: u16, value: &Value) -> Response<Body> {
    response(status, "application/json", "no-store", Body::Full(Some(Bytes::from(value.to_string()))))
}

fn error_response(error: &ApiError) -> Response<Body> {
    json_response(error.status, &error.body())
}

fn header(request: &Request<Incoming>, name: impl hyper::header::AsHeaderName) -> Option<&str> {
    request.headers().get(name).and_then(|value| value.to_str().ok())
}

/// The host name of a Host header, without its port.
pub fn host_name(host: &str) -> String {
    let host = host.to_ascii_lowercase();
    if host.starts_with('[') {
        return host[..host.find(']').map_or(host.len(), |end| end + 1)].to_owned();
    }
    match host.rsplit_once(':') {
        Some((name, _)) => name.to_owned(),
        None => host,
    }
}

/// Same-origin rule from serve.py: an Origin header must be exactly `scheme://<Host>`.
pub fn origin_allowed(origin: Option<&str>, host: Option<&str>, fetch_site: Option<&str>) -> bool {
    if let Some(origin) = origin {
        let Some(netloc) = origin.strip_prefix("http://").or_else(|| origin.strip_prefix("https://")) else { return false };
        if netloc.contains(['/', '?', '#']) || Some(netloc) != host {
            return false;
        }
    }
    fetch_site != Some("cross-site")
}

pub async fn handle(shared: Arc<Shared>, request: Request<Incoming>) -> Result<Response<Body>, Abort> {
    shared.stats.requests.fetch_add(1, Ordering::Relaxed);
    if let Some(allowed) = &shared.allowed_host {
        if header(&request, HOST).map(host_name).as_deref() != Some(allowed.as_str()) {
            return Ok(error_response(&ApiError::new(421, "Unknown host")));
        }
    }
    match *request.method() {
        Method::POST => post(shared, request).await,
        Method::GET | Method::HEAD => Ok(get(shared, request).await),
        _ => Ok(error_response(&ApiError::new(405, "Method not allowed"))),
    }
}

enum BodyError {
    Api(ApiError),
    /// The body did not arrive in time: close the connection without a response.
    Timeout,
}

impl From<ApiError> for BodyError {
    fn from(error: ApiError) -> BodyError {
        BodyError::Api(error)
    }
}

async fn read_body(shared: &Shared, request: Request<Incoming>, limit: usize) -> Result<Value, BodyError> {
    let content_type = header(&request, CONTENT_TYPE).unwrap_or("text/plain");
    if !content_type.split(';').next().unwrap_or("").trim().eq_ignore_ascii_case("application/json") {
        return Err(ApiError::new(415, "Send JSON").into());
    }
    let length = match header(&request, "content-length") {
        None => return Err(ApiError::new(413, "Request is too large").into()),
        Some(text) => text.trim().parse::<usize>().map_err(|_| ApiError::new(400, "Invalid JSON"))?,
    };
    if length > limit {
        return Err(ApiError::new(413, "Request is too large").into());
    }
    let Ok(_budget) = shared.body_budget.try_acquire_many(length as u32) else {
        return Err(ApiError::new(503, "The table server is busy. Try again.").into());
    };
    let timeout = shared.lock().config.body_timeout;
    let mut body = request.into_body();
    // Grows with what actually arrives; a Content-Length alone reserves nothing.
    let mut data = Vec::with_capacity(length.min(64 * 1024));
    let read = async {
        while let Some(frame) = poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await {
            let frame = frame.map_err(|_| ApiError::new(400, "Invalid JSON"))?;
            if let Ok(chunk) = frame.into_data() {
                if data.len() + chunk.len() > limit {
                    return Err(ApiError::new(413, "Request is too large"));
                }
                data.extend_from_slice(&chunk);
            }
        }
        Ok(())
    };
    match tokio::time::timeout(timeout, read).await {
        Ok(result) => result?,
        Err(_) => return Err(BodyError::Timeout),
    }
    let value: Value = serde_json::from_slice(&data).map_err(|_| ApiError::new(400, "Invalid JSON"))?;
    if !value.is_object() {
        return Err(ApiError::new(400, "Expected a JSON object").into());
    }
    Ok(value)
}

async fn post(shared: Arc<Shared>, request: Request<Incoming>) -> Result<Response<Body>, Abort> {
    let path = request.uri().path().to_owned();
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    let token = header(&request, "authorization").and_then(|value| value.strip_prefix("Bearer ")).unwrap_or("").to_owned();
    let allowed = origin_allowed(header(&request, "origin"), header(&request, HOST), header(&request, "sec-fetch-site"));
    // Anyone may create or join with a small body; everything else needs a seat before its
    // (up to 1 MiB) body is read or parsed.
    let checked = match parts[..] {
        _ if !allowed => Err(ApiError::new(403, "Requests must come from this website")),
        ["api", "rooms"] | ["api", "rooms", _, "join"] => Ok(MAX_JOIN_BODY),
        ["api", "rooms", room_id, action] => shared.lock().check_seat(room_id, &token).map(|()| if action == "relay" { MAX_RELAY_BODY } else { MAX_BODY }),
        _ => Err(ApiError::new(404, "Unknown endpoint")),
    };
    let body = match checked {
        Ok(limit) => read_body(&shared, request, limit).await,
        Err(error) => Err(error.into()),
    };
    let (result, actor, delay) = match body {
        Ok(Value::Object(body)) => {
            let outcome = shared.lock().post(&parts, &token, &body, Instant::now());
            shared.notify();
            (outcome.result, outcome.actor, outcome.delay)
        }
        Ok(_) => unreachable!("read_body returns objects"),
        Err(BodyError::Timeout) => return Err(Abort),
        Err(BodyError::Api(error)) => {
            let delay = {
                let mut state = shared.lock();
                let State { config, rng, .. } = &mut *state;
                config.delay(rng)
            };
            (Err(error), None, delay)
        }
    };
    if !hold(&shared, actor.as_ref(), delay).await {
        return Err(Abort);
    }
    Ok(match result {
        Ok(value) => json_response(200, &value),
        Err(error) => error_response(&error),
    })
}

/// Fault injection: delay a response. False means the connection was dropped meanwhile.
async fn hold(shared: &Shared, actor: Option<&Actor>, delay: Duration) -> bool {
    if delay.is_zero() {
        return true;
    }
    let deadline = tokio::time::Instant::now() + delay;
    let mut changed = shared.changed.subscribe();
    loop {
        changed.borrow_and_update();
        if actor.is_some_and(|actor| shared.lock().dropped(actor)) {
            return false;
        }
        tokio::select! {
            _ = changed.changed() => {}
            _ = tokio::time::sleep_until(deadline) => return true,
        }
    }
}

fn parse_id(text: Option<&str>) -> Result<u64, ApiError> {
    match text {
        None | Some("") => Ok(0),
        Some(text) if text.len() <= 16 && text.bytes().all(|b| b.is_ascii_digit()) => Ok(text.parse().expect("digits")),
        Some(_) => Err(ApiError::new(400, "Invalid event id")),
    }
}

async fn get(shared: Arc<Shared>, request: Request<Incoming>) -> Response<Body> {
    let path = request.uri().path();
    if request.method() == Method::GET && path == "/api/health" {
        return json_response(200, &json!({ "ok": true }));
    }
    if !path.starts_with("/api/") {
        return static_file(&shared, path, request.method() == Method::HEAD).await;
    }
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    let ["api", "rooms", room_id, "events"] = parts[..] else {
        return error_response(&ApiError::new(404, "Unknown endpoint"));
    };
    if request.method() != Method::GET {
        return error_response(&ApiError::new(405, "Method not allowed"));
    }
    let query = request.uri().query().unwrap_or("");
    let after = match (parse_id(util::query_value(query, "after").as_deref()), request.headers().get("last-event-id")) {
        (Ok(after), None) => Ok(after),
        (Ok(after), Some(value)) => value.to_str().map_err(|_| ApiError::new(400, "Invalid event id")).and_then(|text| parse_id(Some(text))).map(|id| id.max(after)),
        (Err(error), _) => Err(error),
    };
    let token = util::query_value(query, "token").unwrap_or_default();
    let start = after.and_then(|after| shared.lock().open_stream(room_id, &token, after, Instant::now()));
    let start = match start {
        Ok(start) => start,
        Err(error) => return error_response(&error),
    };
    shared.notify();
    let (sender, receiver) = mpsc::channel(16);
    tokio::spawn(run_stream(shared.clone(), room_id.to_owned(), start.seat, start.stream, sender));
    let mut response = response(200, "text/event-stream", "no-store", Body::Stream(receiver));
    response.headers_mut().insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
}

async fn run_stream(shared: Arc<Shared>, room: String, seat: usize, mut stream: StreamState, sender: mpsc::Sender<Result<Bytes, Abort>>) {
    let mut changed = shared.changed.subscribe();
    if sender.send(Ok(Bytes::from_static(RETRY.as_bytes()))).await.is_ok() {
        stream.last_write = Instant::now();
        loop {
            changed.borrow_and_update();
            let step = shared.lock().poll_stream(&room, seat, &mut stream, Instant::now());
            match step {
                Step::Send { text, finished } => {
                    if sender.send(Ok(Bytes::from(text))).await.is_err() {
                        break;
                    }
                    stream.last_write = Instant::now();
                    shared.lock().stream_written(&room, seat, &stream, stream.last_write);
                    if finished {
                        break;
                    }
                }
                Step::Wait(until) => {
                    tokio::select! {
                        _ = changed.changed() => {}
                        _ = tokio::time::sleep_until(until.into()) => {}
                        _ = sender.closed() => break,
                    }
                }
                Step::Abort => {
                    let _ = sender.send(Err(Abort)).await;
                    break;
                }
                Step::End => break,
            }
        }
    }
    shared.lock().stream_ended(&room, seat, &stream, Instant::now());
    shared.notify();
}

pub fn mime(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "wasm" => "application/wasm",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "webmanifest" => "application/manifest+json",
        "txt" | "ts" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Map a URL path to a relative file path, refusing traversal and hidden files.
pub fn safe_path(path: &str) -> Option<PathBuf> {
    let decoded = util::percent_decode(path, false)?;
    let mut relative = PathBuf::new();
    for segment in decoded.split('/').filter(|segment| !segment.is_empty()) {
        if segment.starts_with('.') || segment.contains(['\\', '\0']) {
            return None;
        }
        relative.push(segment);
    }
    if decoded.ends_with('/') {
        relative.push("index.html");
    }
    Some(relative)
}

async fn static_file(shared: &Shared, path: &str, head: bool) -> Response<Body> {
    let not_found = || error_response(&ApiError::new(404, "Not found"));
    let Some(relative) = safe_path(path) else { return not_found() };
    let root = shared.web_dir.clone();
    let file = tokio::task::spawn_blocking(move || -> Option<(PathBuf, Vec<u8>)> {
        let path = root.join(&relative).canonicalize().ok()?;
        if !path.starts_with(&root) || !path.is_file() {
            return None;
        }
        std::fs::read(&path).ok().map(|bytes| (path, bytes))
    })
    .await
    .ok()
    .flatten();
    let Some((path, bytes)) = file else { return not_found() };
    let length = bytes.len();
    let body = if head { Body::Full(None) } else { Body::Full(Some(Bytes::from(bytes))) };
    let mut response = response(200, mime(&path), "no-cache", body);
    if head {
        response.headers_mut().insert("content-length", HeaderValue::from(length));
    }
    response
}

/// Background work: expiry, liveness and the drop fault.
pub async fn maintain(shared: Arc<Shared>) -> Infallible {
    let mut interval = tokio::time::interval(Duration::from_millis(100));
    loop {
        interval.tick().await;
        if shared.lock().cleanup(Instant::now()) {
            shared.notify();
        }
    }
}

pub async fn drop_fault(shared: Arc<Shared>, every: Duration) -> Infallible {
    let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    loop {
        interval.tick().await;
        if shared.lock().drop_random() {
            shared.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_rule_matches_serve_py() {
        let host = Some("abc.onion");
        assert!(origin_allowed(None, host, None));
        assert!(origin_allowed(Some("http://abc.onion"), host, Some("same-origin")));
        assert!(origin_allowed(Some("https://abc.onion"), host, None));
        assert!(!origin_allowed(Some("http://evil.example"), host, None));
        assert!(!origin_allowed(Some("null"), host, None));
        assert!(!origin_allowed(Some("http://abc.onion/"), host, None));
        assert!(!origin_allowed(Some("ftp://abc.onion"), host, None));
        assert!(!origin_allowed(None, host, Some("cross-site")));
    }

    #[test]
    fn host_names_drop_ports() {
        assert_eq!(host_name("ABC.onion:80"), "abc.onion");
        assert_eq!(host_name("abc.onion"), "abc.onion");
        assert_eq!(host_name("[::1]:8080"), "[::1]");
    }

    #[test]
    fn static_paths_refuse_traversal() {
        assert_eq!(safe_path("/"), Some(PathBuf::from("index.html")));
        assert_eq!(safe_path("/pkg/p2skat.js"), Some(PathBuf::from("pkg/p2skat.js")));
        assert_eq!(safe_path("/pkg/"), Some(PathBuf::from("pkg/index.html")));
        for path in ["/../x", "/%2e%2e/x", "/pkg/..%2f..%2fx", "/..%5cx", "/.git/config", "/a%00b", "/%ff"] {
            assert_eq!(safe_path(path), None, "{path}");
        }
        assert_eq!(mime(Path::new("a.wasm")), "application/wasm");
        assert_eq!(mime(Path::new("a.mjs")), "text/javascript; charset=utf-8");
    }
}
