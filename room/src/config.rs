//! Command-line options (deployment) and environment knobs (tuning, tests).

use std::path::PathBuf;
use std::time::Duration;

use crate::util::Rng;

pub const USAGE: &str = "\
p2skat-room: serve the p2skat browser app and its three-player rooms.

Usage: p2skat-room [options]

  --bind ADDR          listen address (default 127.0.0.1:8080)
  --web-dir DIR        static files to serve (default ./web)
  --allowed-host HOST  only answer requests whose Host header names HOST
  --max-rooms N        maximum number of tables (default 64)
  --max-connections N  maximum number of open connections (default 256)
  --relay-only         refuse to create WebRTC tables (they have no liveness check)
  --log                print counts (never tokens, room ids or payloads) every minute
  --help               show this text

Tuning and test-only knobs are environment variables shared with scripts/serve.py:
P2SKAT_HEARTBEAT_MS, P2SKAT_LIVENESS_MS, P2SKAT_ROOM_TTL_MS, P2SKAT_CLOSED_RETAIN_MS,
P2SKAT_RATE, P2SKAT_BURST, P2SKAT_LOG_EVENTS, P2SKAT_LOG_BYTES, P2SKAT_TOTAL_BYTES,
P2SKAT_BODY_TIMEOUT_MS, P2SKAT_RELAY_ONLY (1 = --relay-only),
P2SKAT_FAULT_LATENCY_MS (MEAN or MEAN,JITTER) and P2SKAT_FAULT_DROP_S.
";

#[derive(Debug)]
pub struct Options {
    pub bind: String,
    pub web_dir: PathBuf,
    pub allowed_host: Option<String>,
    pub max_rooms: usize,
    pub max_connections: usize,
    pub relay_only: bool,
    pub log: bool,
}

impl Options {
    /// Ok(None) means: print the usage and exit successfully.
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Option<Options>, String> {
        let mut options = Options {
            bind: "127.0.0.1:8080".into(),
            web_dir: PathBuf::from("web"),
            allowed_host: None,
            max_rooms: 64,
            max_connections: 256,
            relay_only: false,
            log: false,
        };
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            let (name, inline) = match arg.split_once('=') {
                Some((name, value)) if name.starts_with("--") => (name.to_owned(), Some(value.to_owned())),
                _ => (arg.clone(), None),
            };
            let mut value = || inline.clone().or_else(|| args.next()).ok_or(format!("{name} needs a value"));
            match name.as_str() {
                "--bind" => options.bind = value()?,
                "--web-dir" => options.web_dir = PathBuf::from(value()?),
                "--allowed-host" => options.allowed_host = Some(value()?.to_ascii_lowercase()),
                "--max-rooms" => {
                    options.max_rooms = value()?.parse().map_err(|_| "--max-rooms needs a number".to_owned())?
                }
                "--max-connections" => match value()?.parse() {
                    Ok(count) if count > 0 => options.max_connections = count,
                    _ => return Err("--max-connections needs a positive number".to_owned()),
                },
                "--relay-only" => options.relay_only = true,
                "--log" => options.log = true,
                "--help" | "-h" => return Ok(None),
                _ => return Err(format!("unknown option {arg}")),
            }
        }
        Ok(Some(options))
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub heartbeat: Duration,
    pub liveness: Duration,
    pub room_ttl: Duration,
    pub closed_retain: Duration,
    pub rate: f64,
    pub burst: f64,
    pub log_events: usize,
    pub log_bytes: usize,
    pub total_bytes: usize,
    /// A request body must arrive completely within this time.
    pub body_timeout: Duration,
    /// Refuse to create WebRTC tables.
    pub relay_only: bool,
    /// Fault injection: mean and jitter of the added latency, in seconds.
    pub latency: f64,
    pub jitter: f64,
    /// Fault injection: seconds between dropped connections (0 = off).
    pub drop: f64,
}

impl Default for Config {
    fn default() -> Config {
        Config::from_lookup(|_| None).expect("defaults are valid")
    }
}

impl Config {
    pub fn from_env() -> Result<Config, String> {
        Config::from_lookup(|name| std::env::var(name).ok())
    }

    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Config, String> {
        let number = |name: &str, default: f64| -> Result<f64, String> {
            match lookup(name).filter(|value| !value.is_empty()) {
                None => Ok(default),
                Some(value) => match value.trim().parse::<f64>() {
                    Ok(number) if number >= 0.0 && number.is_finite() => Ok(number),
                    _ => Err(format!("{name} must be a non-negative number")),
                },
            }
        };
        let ms = |name: &str, default: f64| number(name, default).map(|ms| Duration::from_secs_f64(ms / 1000.0));
        let count = |name: &str, default: usize| number(name, default as f64).map(|value| value as usize);
        let (latency, jitter) = match lookup("P2SKAT_FAULT_LATENCY_MS").filter(|value| !value.is_empty()) {
            None => (0.0, 0.0),
            Some(value) => {
                let parse = |text: &str| match text.trim().parse::<f64>() {
                    Ok(number) if number >= 0.0 && number.is_finite() => Ok(number / 1000.0),
                    _ => Err("P2SKAT_FAULT_LATENCY_MS must be MEAN or MEAN,JITTER in ms".to_owned()),
                };
                match value.split_once(',') {
                    Some((mean, jitter)) => (parse(mean)?, parse(jitter)?),
                    None => {
                        let mean = parse(&value)?;
                        (mean, mean / 2.0)
                    }
                }
            }
        };
        let relay_only = match lookup("P2SKAT_RELAY_ONLY").as_deref().map(str::trim) {
            None | Some("" | "0") => false,
            Some("1") => true,
            Some(_) => return Err("P2SKAT_RELAY_ONLY must be 0 or 1".to_owned()),
        };
        Ok(Config {
            heartbeat: ms("P2SKAT_HEARTBEAT_MS", 10_000.0)?,
            liveness: ms("P2SKAT_LIVENESS_MS", 120_000.0)?,
            room_ttl: ms("P2SKAT_ROOM_TTL_MS", 3_600_000.0)?,
            closed_retain: ms("P2SKAT_CLOSED_RETAIN_MS", 30_000.0)?,
            rate: number("P2SKAT_RATE", 50.0)?,
            burst: number("P2SKAT_BURST", 50.0)?,
            log_events: count("P2SKAT_LOG_EVENTS", 256)?,
            log_bytes: count("P2SKAT_LOG_BYTES", 1 << 20)?,
            total_bytes: count("P2SKAT_TOTAL_BYTES", 16 << 20)?,
            body_timeout: ms("P2SKAT_BODY_TIMEOUT_MS", 20_000.0)?,
            relay_only,
            latency,
            jitter,
            drop: number("P2SKAT_FAULT_DROP_S", 0.0)?,
        })
    }

    /// One latency sample (zero unless the latency fault is on).
    pub fn delay(&self, rng: &mut Rng) -> Duration {
        if self.latency == 0.0 {
            return Duration::ZERO;
        }
        let sample = self.latency + (rng.unit() * 2.0 - 1.0) * self.jitter;
        Duration::from_secs_f64(sample.max(0.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn options_parse_with_defaults() {
        let options = Options::parse(args(&[])).unwrap().unwrap();
        assert_eq!((options.bind.as_str(), options.max_rooms, options.log), ("127.0.0.1:8080", 64, false));
        assert_eq!((options.max_connections, options.relay_only), (256, false));
        let options = Options::parse(args(&["--bind=0.0.0.0:1", "--allowed-host", "X.onion", "--max-rooms", "8", "--log", "--max-connections=64", "--relay-only"]))
            .unwrap()
            .unwrap();
        assert_eq!((options.max_connections, options.relay_only), (64, true));
        assert!(Options::parse(args(&["--max-connections", "0"])).is_err());
        assert_eq!(options.bind, "0.0.0.0:1");
        assert_eq!(options.allowed_host.as_deref(), Some("x.onion"));
        assert_eq!(options.max_rooms, 8);
        assert!(options.log);
        assert!(Options::parse(args(&["--help"])).unwrap().is_none());
        assert!(Options::parse(args(&["--max-rooms"])).is_err());
        assert!(Options::parse(args(&["--nope"])).is_err());
    }

    #[test]
    fn environment_knobs() {
        let config = Config::default();
        assert_eq!(config.heartbeat, Duration::from_secs(10));
        assert_eq!(config.liveness, Duration::from_secs(120));
        assert_eq!((config.log_events, config.log_bytes, config.total_bytes), (256, 1 << 20, 16 << 20));
        assert_eq!(config.delay(&mut Rng::new()), Duration::ZERO);
        assert_eq!((config.body_timeout, config.relay_only), (Duration::from_secs(20), false));
        assert!(Config::from_lookup(|name| (name == "P2SKAT_RELAY_ONLY").then(|| "1".into())).unwrap().relay_only);
        assert!(Config::from_lookup(|name| (name == "P2SKAT_RELAY_ONLY").then(|| "yes".into())).is_err());
        let config = Config::from_lookup(|name| match name {
            "P2SKAT_HEARTBEAT_MS" => Some("300".into()),
            "P2SKAT_FAULT_LATENCY_MS" => Some("800,400".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(config.heartbeat, Duration::from_millis(300));
        let mut rng = Rng::new();
        for _ in 0..100 {
            let delay = config.delay(&mut rng).as_secs_f64();
            assert!((0.4..=1.2).contains(&delay));
        }
        assert!(Config::from_lookup(|name| (name == "P2SKAT_RATE").then(|| "-1".into())).is_err());
        assert!(Config::from_lookup(|name| (name == "P2SKAT_FAULT_LATENCY_MS").then(|| "x".into())).is_err());
    }
}
