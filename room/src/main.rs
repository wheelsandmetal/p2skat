//! p2skat-room: static files plus ephemeral three-player rooms (WebRTC
//! signaling or an end-to-end encrypted frame relay), sized for a router.

mod config;
mod http;
mod rooms;
mod util;

use std::io::Write as _;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

use config::{Config, Options, USAGE};
use http::Shared;
use rooms::State;

/// Router-sized: 8 tables × 3 players need a few dozen connections.
const MAX_CONNECTIONS: usize = 256;

fn main() {
    let options = match Options::parse(std::env::args().skip(1)) {
        Ok(Some(options)) => options,
        Ok(None) => {
            print!("{USAGE}");
            return;
        }
        Err(error) => {
            eprintln!("p2skat-room: {error}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    let config = Config::from_env().unwrap_or_else(|error| {
        eprintln!("p2skat-room: {error}");
        std::process::exit(2);
    });
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    if let Err(error) = runtime.block_on(serve(options, config)) {
        eprintln!("p2skat-room: {error}");
        std::process::exit(1);
    }
}

async fn serve(options: Options, config: Config) -> std::io::Result<()> {
    let web_dir = options.web_dir.canonicalize().map_err(|error| std::io::Error::new(error.kind(), format!("--web-dir {}: {error}", options.web_dir.display())))?;
    let drop_every = config.drop;
    let shared = Arc::new(Shared::new(State::new(config, options.max_rooms), web_dir, options.allowed_host));
    let listener = TcpListener::bind(&options.bind).await?;
    println!("p2skat-room: http://{}", listener.local_addr()?);
    std::io::stdout().flush()?;

    tokio::spawn(http::maintain(shared.clone()));
    if drop_every > 0.0 {
        tokio::spawn(http::drop_fault(shared.clone(), Duration::from_secs_f64(drop_every)));
    }
    if options.log {
        tokio::spawn(log_counts(shared.clone()));
    }

    let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(_) => {
                // Usually out of file descriptors; back off instead of spinning.
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let Ok(permit) = connections.clone().try_acquire_owned() else { continue };
        let _ = stream.set_nodelay(true);
        let shared = shared.clone();
        tokio::spawn(async move {
            let service = service_fn(move |request| http::handle(shared.clone(), request));
            let _ = http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(Duration::from_secs(15))
                .serve_connection(TokioIo::new(stream), service)
                .await;
            drop(permit);
        });
    }
}

/// Counts only: never tokens, room ids, names or payloads.
async fn log_counts(shared: Arc<Shared>) {
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    loop {
        interval.tick().await;
        let (rooms, streams, frames, bytes) = {
            let state = shared.lock();
            (state.rooms.len(), state.streams(), state.frames, state.total_bytes)
        };
        let requests = shared.stats.requests.load(Ordering::Relaxed);
        println!("p2skat-room: rooms={rooms} streams={streams} requests={requests} frames={frames} queued_bytes={bytes}");
    }
}
