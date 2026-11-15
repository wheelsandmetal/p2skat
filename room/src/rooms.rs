//! Room state: seats, signaling, relay frames and per-player acked event logs.
//!
//! Mirrors `scripts/serve.py`; `scripts/test_serve.py` checks both. Nothing
//! here blocks or does I/O, so the HTTP layer holds the lock only briefly.

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::config::Config;
use crate::util::{self, Rng};

pub const MAX_SIGNAL: usize = 64 * 1024;
pub const MAX_BODY: usize = MAX_SIGNAL * 2 + 4096;
/// Create and join need no token, so their bodies stay small.
pub const MAX_JOIN_BODY: usize = 4096;
pub const MAX_RELAY_BODY: usize = 1024 * 1024 + 4096;
pub const MAX_PAYLOAD: usize = 64 * 1024 + 64;
pub const MAX_SEQ: u64 = (1 << 53) - 1;
pub const RETRY: &str = "retry: 2000\n\n";
const PING: &str = "event: ping\ndata: {\"type\":\"ping\"}\n\n";

#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
    pub code: &'static str,
    pub expected: Option<Value>,
}

impl ApiError {
    pub fn new(status: u16, message: impl Into<String>) -> ApiError {
        let code = match status {
            400 => "bad_request",
            401 => "unauthorized",
            403 => "forbidden",
            404 => "not_found",
            405 => "method_not_allowed",
            409 => "conflict",
            413 => "too_large",
            415 => "unsupported_media_type",
            421 => "wrong_host",
            422 => "gap",
            429 => "over_limit",
            503 => "busy",
            _ => "error",
        };
        ApiError { status, message: message.into(), code, expected: None }
    }

    pub fn code(mut self, code: &'static str) -> ApiError {
        self.code = code;
        self
    }

    pub fn body(&self) -> Value {
        let mut body = json!({ "error": self.message, "code": self.code });
        if let Some(expected) = &self.expected {
            body["expected"] = expected.clone();
        }
        body
    }
}

fn bad(message: &str) -> ApiError {
    ApiError::new(400, message)
}

fn gone() -> ApiError {
    ApiError::new(404, "This table ended or expired")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    WebRtc,
    Relay,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::WebRtc => "webrtc",
            Mode::Relay => "relay",
        }
    }

    fn parse(value: &Value) -> Result<Mode, ApiError> {
        match value.as_str() {
            Some("webrtc") => Ok(Mode::WebRtc),
            Some("relay") => Ok(Mode::Relay),
            _ => Err(bad("Mode must be webrtc or relay")),
        }
    }
}

pub struct Event {
    pub id: u64,
    pub data: String,
    pub ready: Instant,
    pub closed: bool,
}

pub struct Player {
    pub seat: usize,
    pub name: String,
    pub browser: String,
    token: String,
    pub log: VecDeque<Event>,
    pub log_bytes: usize,
    pub next_id: u64,
    /// Highest event id written to the current stream.
    pub delivered: u64,
    /// Highest event id discarded before the player acknowledged it.
    pub lost: u64,
    pub generation: u64,
    /// Generation of the attached stream, 0 if none.
    pub stream: u64,
    pub detached_since: Instant,
    /// Last authenticated POST (relay tables: the client's keepalive proves the tab still runs).
    pub last_seen: Instant,
    pub drop_epoch: u64,
    last_ready: Instant,
    tokens: f64,
    refilled: Instant,
    /// Last relay seq accepted from this player, per destination seat.
    sent: [u64; 3],
}

impl Player {
    fn public(&self) -> Value {
        json!({ "seat": self.seat, "name": self.name, "browser": self.browser })
    }
}

pub struct Room {
    pub mode: Mode,
    pub players: Vec<Player>,
    pub updated: Instant,
    pub closed: bool,
    signaled: [[bool; 3]; 3],
    roster_version: u64,
}

impl Room {
    fn roster(&self) -> Value {
        Value::Array(self.players.iter().map(Player::public).collect())
    }

    fn authorize(&self, token: &str) -> Result<usize, ApiError> {
        if token.len() > 128 || !token.is_ascii() {
            return Err(ApiError::new(401, "Invalid player token"));
        }
        let mut found = None;
        for player in &self.players {
            if util::constant_time_eq(player.token.as_bytes(), token.as_bytes()) {
                found = Some(player.seat);
            }
        }
        found.ok_or_else(|| ApiError::new(401, "Invalid player token"))
    }

    fn expected(&self, seat: usize) -> Value {
        let sent = &self.players[seat].sent;
        let map: Map<String, Value> = self
            .players
            .iter()
            .filter(|other| other.seat != seat)
            .map(|other| (other.seat.to_string(), json!(sent[other.seat] + 1)))
            .collect();
        Value::Object(map)
    }
}

/// Who a POST acted as, so fault injection can drop that player's connections.
pub struct Actor {
    pub room: String,
    pub seat: usize,
    pub epoch: u64,
}

pub struct Outcome {
    pub result: Result<Value, ApiError>,
    pub actor: Option<Actor>,
    pub delay: Duration,
}

pub struct StreamStart {
    pub seat: usize,
    pub stream: StreamState,
}

pub struct StreamState {
    pub cursor: u64,
    /// None for a closed table: replay its closed event, never supersede.
    pub generation: Option<u64>,
    pub epoch: u64,
    roster_sent: Option<u64>,
    pub last_write: Instant,
}

pub enum Step {
    Send { text: String, finished: bool },
    Wait(Instant),
    /// Superseded or the table is gone: end the response normally.
    End,
    /// Fault injection: cut the connection.
    Abort,
}

pub struct State {
    pub rooms: HashMap<String, Room>,
    pub total_bytes: usize,
    pub config: Config,
    pub max_rooms: usize,
    pub rng: Rng,
    pub frames: u64,
}

struct Logs<'a> {
    total: &'a mut usize,
    /// Each table's fair share of `config.total_bytes`, so one table cannot crowd out the others.
    room_cap: usize,
    config: &'a Config,
    rng: &'a mut Rng,
    now: Instant,
}

impl Logs<'_> {
    fn discard(&mut self, player: &mut Player) -> u64 {
        let event = player.log.pop_front().expect("log entry");
        player.log_bytes -= event.data.len();
        *self.total -= event.data.len();
        event.id
    }

    fn acknowledge(&mut self, player: &mut Player, upto: u64) {
        while player.log.front().is_some_and(|event| event.id <= upto) {
            self.discard(player);
        }
    }

    /// `others`: bytes the rest of the table holds; None (the closed event) bypasses every limit.
    fn append(&mut self, player: &mut Player, event: &Value, others: Option<usize>) -> Result<(), String> {
        let data = event.to_string();
        let size = data.len();
        let config = self.config;
        if let Some(others) = others {
            let room_cap = self.room_cap;
            // Events already written to a stream go first; undelivered ones never do.
            while (player.log.len() >= config.log_events
                || player.log_bytes + size > config.log_bytes
                || others + player.log_bytes + size > room_cap
                || *self.total + size > config.total_bytes)
                && player.log.front().is_some_and(|event| event.id <= player.delivered)
            {
                let id = self.discard(player);
                player.lost = player.lost.max(id);
            }
            if player.log.len() >= config.log_events || player.log_bytes + size > config.log_bytes {
                return Err(format!("{}’s connection fell too far behind. Create a new table.", player.name));
            }
            if others + player.log_bytes + size > room_cap {
                return Err("This table ran out of message space. Create a new table.".into());
            }
            if *self.total + size > config.total_bytes {
                return Err("The table server is out of message space. Create a new table.".into());
            }
        }
        let ready = if config.latency > 0.0 {
            (self.now + config.delay(self.rng)).max(player.last_ready)
        } else {
            self.now
        };
        player.last_ready = ready;
        player.log.push_back(Event { id: player.next_id, data, ready, closed: event["type"] == "closed" });
        player.next_id += 1;
        player.log_bytes += size;
        *self.total += size;
        Ok(())
    }

    fn close(&mut self, room: &mut Room, message: &str) {
        if room.closed {
            return;
        }
        room.closed = true;
        room.updated = self.now;
        let event = json!({ "type": "closed", "message": message });
        for player in &mut room.players {
            while !player.log.is_empty() {
                self.discard(player);
            }
            // The terminal event bypasses every limit.
            let _ = self.append(player, &event, None);
        }
    }

    /// Append, closing the room if a limit is hit.
    fn emit(&mut self, room: &mut Room, to: usize, event: &Value) -> Result<(), ApiError> {
        let others = room.players.iter().filter(|player| player.seat != to).map(|player| player.log_bytes).sum();
        match self.append(&mut room.players[to], event, Some(others)) {
            Ok(()) => Ok(()),
            Err(message) => {
                self.close(room, &message);
                Err(ApiError::new(429, message))
            }
        }
    }
}

fn int(value: &Value) -> Option<u64> {
    value.as_u64()
}

impl State {
    pub fn new(config: Config, max_rooms: usize) -> State {
        State { rooms: HashMap::new(), total_bytes: 0, config, max_rooms, rng: Rng::new(), frames: 0 }
    }

    fn split(&mut self, now: Instant) -> (&mut HashMap<String, Room>, Logs<'_>) {
        let State { rooms, total_bytes, config, rng, max_rooms, .. } = self;
        let room_cap = config.total_bytes / (*max_rooms).max(1);
        (rooms, Logs { total: total_bytes, room_cap, config, rng, now })
    }

    pub fn cleanup(&mut self, now: Instant) -> bool {
        let (rooms, mut logs) = self.split(now);
        let config = logs.config.clone();
        let mut changed = false;
        let mut expired = Vec::new();
        for (id, room) in rooms.iter_mut() {
            let idle = now.saturating_duration_since(room.updated);
            if room.closed {
                if idle > config.closed_retain {
                    expired.push(id.clone());
                }
                continue;
            }
            if idle > config.room_ttl {
                logs.close(room, "This table expired. Create a new one.");
                changed = true;
                continue;
            }
            if room.mode == Mode::Relay {
                // A frozen tab can keep its stream open, so its keepalive POSTs count too.
                let lost = room
                    .players
                    .iter()
                    .find(|player| {
                        (player.stream == 0 && now.saturating_duration_since(player.detached_since) > config.liveness)
                            || now.saturating_duration_since(player.last_seen) > config.liveness
                    })
                    .map(|player| format!("{} lost connection.", player.name));
                if let Some(message) = lost {
                    logs.close(room, &message);
                    changed = true;
                }
            }
        }
        for id in expired {
            if let Some(mut room) = rooms.remove(&id) {
                for player in &mut room.players {
                    while !player.log.is_empty() {
                        logs.discard(player);
                    }
                }
                changed = true;
            }
        }
        changed
    }

    fn join(&mut self, room_id: &str, body: &Map<String, Value>, now: Instant) -> Result<Value, ApiError> {
        if !body.contains_key("name") || !body.contains_key("browser") || body.keys().any(|key| !["name", "browser", "mode"].contains(&key.as_str())) {
            return Err(bad("Supply your name and browser identifier"));
        }
        let name = body["name"].as_str().unwrap_or("");
        let trimmed = name.trim();
        if body["name"].as_str().is_none() || !(1..=40).contains(&trimmed.chars().count()) || !name.chars().all(util::printable) {
            return Err(bad("Names must have 1–40 readable characters"));
        }
        let browser = match body["browser"].as_str() {
            Some(browser) if (1..=48).contains(&browser.len()) && browser.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') => browser,
            _ => return Err(bad("Invalid browser identifier")),
        };
        let room = self.rooms.get_mut(room_id).ok_or_else(gone)?;
        if let Some(mode) = body.get("mode") {
            if Mode::parse(mode)? != room.mode {
                return Err(bad(&format!("This table uses {} mode", room.mode.name())).code("wrong_mode"));
            }
        }
        if room.players.len() == 3 {
            return Err(ApiError::new(409, "This table already has three players").code("room_full"));
        }
        let player = Player {
            seat: room.players.len(),
            name: trimmed.to_owned(),
            browser: browser.to_owned(),
            token: util::token(),
            log: VecDeque::new(),
            log_bytes: 0,
            next_id: 1,
            delivered: 0,
            lost: 0,
            generation: 0,
            stream: 0,
            detached_since: now,
            last_seen: now,
            drop_epoch: 0,
            last_ready: now,
            tokens: self.config.burst,
            refilled: now,
            sent: [0; 3],
        };
        let (seat, token) = (player.seat, player.token.clone());
        room.players.push(player);
        room.updated = now;
        room.roster_version += 1;
        Ok(json!({ "room": room_id, "seat": seat, "token": token, "players": room.roster(), "mode": room.mode.name() }))
    }

    fn create(&mut self, body: &Map<String, Value>, now: Instant) -> Result<Value, ApiError> {
        if !body.contains_key("name") || !body.contains_key("browser") || body.keys().any(|key| !["name", "browser", "mode"].contains(&key.as_str())) {
            return Err(bad("Supply your name and browser identifier"));
        }
        let mode = body.get("mode").map(Mode::parse).transpose()?.unwrap_or(Mode::WebRtc);
        if mode == Mode::WebRtc && self.config.relay_only {
            return Err(bad("This table server only hosts tables that route the game through it").code("relay_only"));
        }
        self.cleanup(now);
        if self.rooms.len() >= self.max_rooms {
            return Err(ApiError::new(503, "Too many tables. Try again later."));
        }
        let id = util::token();
        let room = Room { mode, players: Vec::new(), updated: now, closed: false, signaled: [[false; 3]; 3], roster_version: 0 };
        self.rooms.insert(id.clone(), room);
        let result = self.join(&id, body, now);
        if result.is_err() {
            self.rooms.remove(&id);
        }
        result
    }

    /// Route an authenticated or anonymous POST. `parts` is the path split on `/`.
    pub fn post(&mut self, parts: &[&str], token: &str, body: &Map<String, Value>, now: Instant) -> Outcome {
        let mut actor = None;
        let result = self.route(parts, token, body, now, &mut actor);
        let delay = self.config.delay(&mut self.rng);
        Outcome { result, actor, delay }
    }

    fn route(&mut self, parts: &[&str], token: &str, body: &Map<String, Value>, now: Instant, actor: &mut Option<Actor>) -> Result<Value, ApiError> {
        match parts {
            ["api", "rooms"] => self.create(body, now),
            ["api", "rooms", room_id, action] => {
                self.cleanup(now);
                let room = self.rooms.get(*room_id).ok_or_else(gone)?;
                if room.closed {
                    return Err(gone().code("room_closed"));
                }
                if *action == "join" {
                    return self.join(room_id, body, now);
                }
                let seat = room.authorize(token)?;
                *actor = Some(Actor { room: room_id.to_string(), seat, epoch: room.players[seat].drop_epoch });
                let (rooms, mut logs) = self.split(now);
                let room = rooms.get_mut(*room_id).expect("room exists");
                room.players[seat].last_seen = now;
                let result = match *action {
                    "signal" => signal(room, seat, body, &mut logs),
                    "relay" => relay(room, seat, body, &mut logs),
                    "leave" => {
                        if !body.is_empty() {
                            return Err(bad("Leave does not accept a payload"));
                        }
                        let message = format!("{} left the table.", room.players[seat].name);
                        logs.close(room, &message);
                        Ok(json!({ "ok": true }))
                    }
                    _ => Err(ApiError::new(404, "Unknown endpoint")),
                };
                if *action == "relay" && result.is_ok() {
                    self.frames += body.get("frames").and_then(Value::as_array).map_or(0, |frames| frames.len() as u64);
                }
                result
            }
            _ => Err(ApiError::new(404, "Unknown endpoint")),
        }
    }

    /// Checked before a POST body is read: the table exists, is open, and the token holds a seat.
    pub fn check_seat(&self, room_id: &str, token: &str) -> Result<(), ApiError> {
        let room = self.rooms.get(room_id).ok_or_else(gone)?;
        if room.closed {
            return Err(gone().code("room_closed"));
        }
        room.authorize(token).map(|_| ())
    }

    pub fn open_stream(&mut self, room_id: &str, token: &str, after: u64, now: Instant) -> Result<StreamStart, ApiError> {
        self.cleanup(now);
        let (rooms, mut logs) = self.split(now);
        let room = rooms.get_mut(room_id).ok_or_else(gone)?;
        let seat = room.authorize(token)?;
        if after >= room.players[seat].next_id {
            return Err(bad("Invalid event id"));
        }
        if !room.closed && after < room.players[seat].lost {
            let message = format!("{} missed messages after a connection problem.", room.players[seat].name);
            logs.close(room, &message);
        }
        let player = &mut room.players[seat];
        let (cursor, generation) = if room.closed {
            (player.log.back().map_or(0, |event| event.id - 1), None)
        } else {
            logs.acknowledge(player, after);
            player.delivered = after;
            player.generation += 1;
            player.stream = player.generation;
            (after, Some(player.generation))
        };
        Ok(StreamStart {
            seat,
            stream: StreamState { cursor, generation, epoch: player.drop_epoch, roster_sent: None, last_write: now },
        })
    }

    pub fn poll_stream(&mut self, room_id: &str, seat: usize, stream: &mut StreamState, now: Instant) -> Step {
        let Some(room) = self.rooms.get(room_id) else { return Step::End };
        let player = &room.players[seat];
        if let Some(generation) = stream.generation {
            if player.drop_epoch != stream.epoch {
                return Step::Abort;
            }
            if player.stream != generation {
                return Step::End;
            }
        }
        let mut text = String::new();
        let mut finished = false;
        if !room.closed && stream.roster_sent != Some(room.roster_version) {
            stream.roster_sent = Some(room.roster_version);
            let _ = write!(text, "data: {}\n\n", json!({ "type": "players", "players": room.roster() }));
        }
        let heartbeat = stream.last_write + self.config.heartbeat;
        let mut wake = heartbeat;
        for event in &player.log {
            if event.id <= stream.cursor {
                continue;
            }
            if event.ready > now {
                wake = wake.min(event.ready);
                break;
            }
            let _ = write!(text, "id: {}\ndata: {}\n\n", event.id, event.data);
            stream.cursor = event.id;
            if event.closed {
                finished = true;
                break;
            }
        }
        if !text.is_empty() {
            return Step::Send { text, finished };
        }
        if now >= heartbeat {
            return Step::Send { text: PING.into(), finished: false };
        }
        Step::Wait(wake)
    }

    pub fn stream_written(&mut self, room_id: &str, seat: usize, stream: &StreamState, now: Instant) {
        if let Some(room) = self.rooms.get_mut(room_id) {
            let player = &mut room.players[seat];
            if stream.generation.is_some_and(|generation| player.stream == generation) {
                player.delivered = player.delivered.max(stream.cursor);
            }
            if !room.closed {
                room.updated = now;
            }
        }
    }

    pub fn stream_ended(&mut self, room_id: &str, seat: usize, stream: &StreamState, now: Instant) {
        if let Some(room) = self.rooms.get_mut(room_id) {
            let player = &mut room.players[seat];
            if stream.generation.is_some_and(|generation| player.stream == generation) {
                player.stream = 0;
                player.detached_since = now;
            }
        }
    }

    /// Fault injection: cut one random attached player's connections.
    pub fn drop_random(&mut self) -> bool {
        let candidates: Vec<(String, usize)> = self
            .rooms
            .iter()
            .filter(|(_, room)| !room.closed)
            .flat_map(|(id, room)| room.players.iter().filter(|player| player.stream != 0).map(|player| (id.clone(), player.seat)))
            .collect();
        if candidates.is_empty() {
            return false;
        }
        let (room, seat) = &candidates[self.rng.below(candidates.len())];
        self.rooms.get_mut(room).expect("room exists").players[*seat].drop_epoch += 1;
        true
    }

    pub fn dropped(&self, actor: &Actor) -> bool {
        self.rooms.get(&actor.room).is_some_and(|room| room.players[actor.seat].drop_epoch != actor.epoch)
    }

    pub fn streams(&self) -> usize {
        self.rooms.values().flat_map(|room| &room.players).filter(|player| player.stream != 0).count()
    }
}

fn signal(room: &mut Room, seat: usize, body: &Map<String, Value>, logs: &mut Logs) -> Result<Value, ApiError> {
    if room.mode != Mode::WebRtc {
        return Err(bad("This table relays messages; it does not use WebRTC").code("wrong_mode"));
    }
    if body.len() != 2 || !body.contains_key("to") || !body.contains_key("signal") {
        return Err(bad("Supply a destination and connection signal"));
    }
    let to = match int(&body["to"]) {
        Some(to) if (to as usize) < room.players.len() && to as usize != seat => to as usize,
        _ => return Err(bad("Invalid destination seat")),
    };
    let text = body["signal"].as_str().ok_or_else(|| bad("Invalid connection signal"))?;
    if text.len() > MAX_SIGNAL {
        return Err(ApiError::new(413, "Connection signal is too large"));
    }
    let signal: Value = serde_json::from_str(text).map_err(|_| bad("Invalid connection signal"))?;
    let names: Vec<Value> = room.players.iter().map(|player| json!(player.name)).collect();
    let valid = room.players.len() == 3
        && signal.as_object().is_some_and(|object| {
            object.len() == 6
                && ["version", "from", "to", "names", "type", "sdp"].iter().all(|key| object.contains_key(*key))
                && int(&object["version"]) == Some(1)
                && int(&object["from"]) == Some(seat as u64)
                && int(&object["to"]) == Some(to as u64)
                && object["names"] == Value::Array(names)
                && object["type"] == if seat < to { "offer" } else { "answer" }
                && object["sdp"].as_str().is_some_and(|sdp| sdp.starts_with("v=0\r\n"))
        });
    if !valid {
        return Err(bad("Connection details do not match this table or sender"));
    }
    if room.signaled[seat][to] {
        return Err(ApiError::new(409, "This pairing already exchanged connection details").code("duplicate_signal"));
    }
    logs.emit(room, to, &json!({ "type": "signal", "from": seat, "signal": text }))?;
    room.signaled[seat][to] = true;
    room.updated = logs.now;
    Ok(json!({ "ok": true }))
}

fn relay(room: &mut Room, seat: usize, body: &Map<String, Value>, logs: &mut Logs) -> Result<Value, ApiError> {
    if room.mode != Mode::Relay {
        return Err(bad("This table uses WebRTC; it does not relay messages").code("wrong_mode"));
    }
    let frames = match body.get("frames") {
        Some(Value::Array(frames)) if body.keys().all(|key| key == "frames" || key == "ack") => frames,
        _ => return Err(bad("Supply a list of frames")),
    };
    let ack = match body.get("ack") {
        None => 0,
        Some(value) => match int(value) {
            Some(ack) if ack < room.players[seat].next_id => ack,
            _ => return Err(bad("Invalid acknowledgement")),
        },
    };
    let mut parsed = Vec::with_capacity(frames.len());
    for frame in frames {
        let frame = match frame.as_object() {
            Some(frame) if frame.len() == 3 && ["to", "seq", "payload"].iter().all(|key| frame.contains_key(*key)) => frame,
            _ => return Err(bad("Frames need exactly to, seq and payload")),
        };
        let to = match int(&frame["to"]) {
            Some(to) if (to as usize) < room.players.len() && to as usize != seat => to as usize,
            _ => return Err(bad("Invalid destination seat")),
        };
        let seq = match int(&frame["seq"]) {
            Some(seq) if (1..=MAX_SEQ).contains(&seq) => seq,
            _ => return Err(bad("Invalid sequence number")),
        };
        let payload = match frame["payload"].as_str() {
            Some(payload) if util::is_base64url(payload) => payload,
            _ => return Err(bad("Payloads must be unpadded base64url")),
        };
        if payload.len() * 3 / 4 > MAX_PAYLOAD {
            return Err(ApiError::new(413, "Frame payload is too large"));
        }
        parsed.push((to, seq, payload));
    }
    let now = logs.now;
    let config = logs.config;
    let player = &mut room.players[seat];
    player.tokens = (player.tokens + now.saturating_duration_since(player.refilled).as_secs_f64() * config.rate).min(config.burst);
    player.refilled = now;
    if parsed.len() as f64 > player.tokens {
        let message = format!("{} sent messages too quickly. Create a new table.", player.name);
        logs.close(room, &message);
        return Err(ApiError::new(429, message));
    }
    player.tokens -= parsed.len() as f64;
    let mut expected = player.sent;
    let mut deliver = Vec::new();
    for &(to, seq, payload) in &parsed {
        if seq == expected[to] + 1 {
            expected[to] = seq;
            deliver.push((to, seq, payload));
        } else if seq > expected[to] + 1 {
            let mut error = ApiError::new(422, "A frame is missing before this one");
            error.expected = Some(room.expected(seat));
            return Err(error);
        }
    }
    logs.acknowledge(&mut room.players[seat], ack);
    for (to, seq, payload) in deliver {
        logs.emit(room, to, &json!({ "type": "relay", "from": seat, "seq": seq, "payload": payload }))?;
    }
    room.players[seat].sent = expected;
    room.updated = now;
    Ok(json!({ "expected": room.expected(seat) }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    struct Table {
        state: State,
        room: String,
        tokens: Vec<String>,
        now: Instant,
    }

    impl Table {
        fn new(mode: &str, config: Config) -> Table {
            let mut state = State::new(config, 8);
            let now = Instant::now();
            let host = state.post(&["api", "rooms"], "", &object(json!({"name": "Ada", "browser": "b0", "mode": mode})), now).result.unwrap();
            let room = host["room"].as_str().unwrap().to_owned();
            let mut tokens = vec![host["token"].as_str().unwrap().to_owned()];
            for name in ["Bert", "Clara"] {
                let player = state.post(&["api", "rooms", &room, "join"], "", &object(json!({"name": name, "browser": "b"})), now).result.unwrap();
                tokens.push(player["token"].as_str().unwrap().to_owned());
            }
            Table { state, room, tokens, now }
        }

        fn relay(&mut self, seat: usize, body: Value) -> Result<Value, ApiError> {
            let room = self.room.clone();
            self.state.post(&["api", "rooms", &room, "relay"], &self.tokens[seat].clone(), &object(body), self.now).result
        }

        fn log(&self, seat: usize) -> Vec<Value> {
            self.state.rooms[&self.room].players[seat].log.iter().map(|event| serde_json::from_str(&event.data).unwrap()).collect()
        }
    }

    fn frame(to: usize, seq: u64) -> Value {
        json!({"to": to, "seq": seq, "payload": "AAAA"})
    }

    #[test]
    fn relay_deduplicates_and_rejects_gaps_atomically() {
        let mut table = Table::new("relay", Config::default());
        assert_eq!(table.relay(0, json!({"frames": [frame(1, 1), frame(1, 2)]})).unwrap(), json!({"expected": {"1": 3, "2": 1}}));
        assert_eq!(table.relay(0, json!({"frames": [frame(1, 2), frame(1, 3)]})).unwrap(), json!({"expected": {"1": 4, "2": 1}}));
        let error = table.relay(0, json!({"frames": [frame(2, 1), frame(1, 5)]})).unwrap_err();
        assert_eq!((error.status, error.code), (422, "gap"));
        assert_eq!(error.body()["expected"], json!({"1": 4, "2": 1}));
        let seqs: Vec<u64> = table.log(1).iter().map(|event| event["seq"].as_u64().unwrap()).collect();
        assert_eq!(seqs, [1, 2, 3]);
        assert!(table.log(2).is_empty());
        assert_eq!(table.log(1)[0], json!({"type": "relay", "from": 0, "seq": 1, "payload": "AAAA"}));
    }

    #[test]
    fn signals_are_rejected_in_relay_rooms_and_relay_in_webrtc_rooms() {
        let mut table = Table::new("relay", Config::default());
        let room = table.room.clone();
        let error = table.state.post(&["api", "rooms", &room, "signal"], &table.tokens[0].clone(), &object(json!({"to": 1, "signal": "{}"})), table.now).result.unwrap_err();
        assert_eq!(error.code, "wrong_mode");
        let mut table = Table::new("webrtc", Config::default());
        assert_eq!(table.relay(0, json!({"frames": []})).unwrap_err().code, "wrong_mode");
    }

    #[test]
    fn undelivered_events_over_the_limit_close_the_room() {
        let config = Config { log_events: 2, ..Config::default() };
        let mut table = Table::new("relay", config);
        let error = table.relay(0, json!({"frames": [frame(1, 1), frame(1, 2), frame(1, 3)]})).unwrap_err();
        assert_eq!((error.status, error.code), (429, "over_limit"));
        assert!(error.message.contains("Bert"));
        assert!(table.state.rooms[&table.room].closed);
        assert_eq!(table.log(2), [json!({"type": "closed", "message": error.message})]);
        assert_eq!(table.relay(0, json!({"frames": []})).unwrap_err().code, "room_closed");
    }

    #[test]
    fn delivered_events_make_room_and_resuming_before_them_closes_the_room() {
        let config = Config { log_events: 2, ..Config::default() };
        let mut table = Table::new("relay", config);
        let room = table.room.clone();
        let start = table.state.open_stream(&room, &table.tokens[1].clone(), 0, table.now).unwrap();
        let mut stream = start.stream;
        for seq in 1..=4 {
            table.relay(0, json!({"frames": [frame(1, seq)]})).unwrap();
            assert!(matches!(table.state.poll_stream(&room, 1, &mut stream, table.now), Step::Send { .. }));
            table.state.stream_written(&room, 1, &stream, table.now);
        }
        assert_eq!(table.state.rooms[&room].players[1].lost, 2);
        table.state.open_stream(&room, &table.tokens[1].clone(), 1, table.now).unwrap();
        assert!(table.state.rooms[&room].closed);
    }

    #[test]
    fn rate_limit_and_byte_limits() {
        let mut table = Table::new("relay", Config { burst: 3.0, rate: 0.0, ..Config::default() });
        table.relay(0, json!({"frames": [frame(1, 1), frame(1, 2), frame(1, 3)]})).unwrap();
        let error = table.relay(0, json!({"frames": [frame(1, 4)]})).unwrap_err();
        assert!(error.message.contains("too quickly"));
    }

    #[test]
    fn one_table_cannot_use_up_another_tables_share() {
        // Eight tables share 1200 bytes: 150 each.
        let mut table = Table::new("relay", Config { total_bytes: 8 * 150, ..Config::default() });
        table.relay(0, json!({"frames": [frame(1, 1)]})).unwrap();
        let error = table.relay(0, json!({"frames": [frame(1, 2), frame(1, 3), frame(1, 4)]})).unwrap_err();
        assert_eq!(error.code, "over_limit");
        assert!(error.message.contains("This table ran out of message space"));
        let host = table.state.post(&["api", "rooms"], "", &object(json!({"name": "Dora", "browser": "b", "mode": "relay"})), table.now).result.unwrap();
        let room = host["room"].as_str().unwrap().to_owned();
        table.state.post(&["api", "rooms", &room, "join"], "", &object(json!({"name": "Emil", "browser": "b"})), table.now).result.unwrap();
        let frames = json!({"frames": [frame(1, 1), frame(1, 2)]});
        let result = table.state.post(&["api", "rooms", &room, "relay"], host["token"].as_str().unwrap(), &object(frames), table.now).result;
        assert_eq!(result.unwrap(), json!({"expected": {"1": 3}}));
    }

    #[test]
    fn relay_only_refuses_webrtc_tables() {
        let mut state = State::new(Config { relay_only: true, ..Config::default() }, 8);
        let now = Instant::now();
        for body in [json!({"name": "Ada", "browser": "b"}), json!({"name": "Ada", "browser": "b", "mode": "webrtc"})] {
            let error = state.post(&["api", "rooms"], "", &object(body), now).result.unwrap_err();
            assert_eq!((error.status, error.code), (400, "relay_only"));
        }
        assert!(state.rooms.is_empty());
        state.post(&["api", "rooms"], "", &object(json!({"name": "Ada", "browser": "b", "mode": "relay"})), now).result.unwrap();
    }

    #[test]
    fn seats_are_checked_before_bodies_are_read() {
        let mut table = Table::new("relay", Config::default());
        let room = table.room.clone();
        assert!(table.state.check_seat(&room, &table.tokens[1]).is_ok());
        assert_eq!(table.state.check_seat(&room, "forged").unwrap_err().code, "unauthorized");
        assert_eq!(table.state.check_seat("unknown", &table.tokens[1]).unwrap_err().code, "not_found");
        table.state.post(&["api", "rooms", &room, "leave"], &table.tokens[0].clone(), &Map::new(), table.now).result.unwrap();
        assert_eq!(table.state.check_seat(&room, &table.tokens[1]).unwrap_err().code, "room_closed");
    }

    #[test]
    fn closing_and_deleting_rooms_releases_memory() {
        let config = Config { closed_retain: Duration::ZERO, ..Config::default() };
        let mut table = Table::new("relay", config);
        table.relay(0, json!({"frames": [frame(1, 1), frame(2, 1)]})).unwrap();
        assert!(table.state.total_bytes > 0);
        let room = table.room.clone();
        table.state.post(&["api", "rooms", &room, "leave"], &table.tokens[2].clone(), &Map::new(), table.now).result.unwrap();
        table.state.cleanup(table.now + Duration::from_millis(1));
        assert!(table.state.rooms.is_empty());
        assert_eq!(table.state.total_bytes, 0);
    }

    #[test]
    fn liveness_closes_relay_rooms_only() {
        let config = Config { liveness: Duration::from_secs(1), ..Config::default() };
        let mut relay = Table::new("relay", config.clone());
        let mut webrtc = Table::new("webrtc", config);
        let later = relay.now + Duration::from_secs(2);
        relay.state.cleanup(later);
        webrtc.state.cleanup(later);
        assert_eq!(relay.log(0), [json!({"type": "closed", "message": "Ada lost connection."})]);
        assert!(!webrtc.state.rooms[&webrtc.room].closed);
    }

    #[test]
    fn an_open_stream_without_keepalive_posts_is_lost() {
        let config = Config { liveness: Duration::from_secs(10), ..Config::default() };
        let mut table = Table::new("relay", config);
        let room = table.room.clone();
        for seat in 0..3 {
            table.state.open_stream(&room, &table.tokens[seat].clone(), 0, table.now).unwrap();
        }
        let start = table.now;
        // Ada and Clara send ack-only keepalives every 4 s; Bert's frozen tab never posts.
        for step in 1..=5 {
            table.now = start + Duration::from_secs(4 * step);
            table.state.cleanup(table.now);
            assert_eq!(table.state.rooms[&room].closed, step >= 3, "step {step}");
            for seat in [0, 2] {
                assert_eq!(table.relay(seat, json!({"frames": []})).is_ok(), step < 3);
            }
        }
        assert_eq!(table.log(0), [json!({"type": "closed", "message": "Bert lost connection."})]);
    }
}
