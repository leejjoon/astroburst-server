use std::net::SocketAddr;
use std::str::FromStr;
use std::time::Duration;

/// Runtime configuration for `astroburst-server`, loaded from environment
/// variables at startup.  All fields have documented defaults that match the
/// PRD (§7.1) and are enforced by `impl Default`.
///
/// Invalid environment values emit a warning to stderr and fall back to the
/// default — the server never refuses to start because of a bad env var.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// `ASTROBURST_BIND` — TCP address to listen on.
    /// Default: `127.0.0.1:8080` (loopback only; access via `ssh -L` tunnel).
    pub bind: SocketAddr,

    /// `ASTROBURST_SESSION_TTL` — idle seconds before a session is evicted.
    /// Default: `900` (15 minutes).
    pub session_ttl: Duration,

    /// `ASTROBURST_SESSION_MAX` — hard limit on concurrent sessions.
    /// Default: `8`.
    pub session_max: usize,

    /// `ASTROBURST_JOBS_MAX` — max concurrent CPU-bound jobs (semaphore slots).
    /// Default: `4`.
    pub jobs_max: usize,

    /// `ASTROBURST_CACHE_MAX_ENTRIES` — per-session LRU slot count.
    /// Default: `32`.
    pub cache_max_entries: usize,

    /// `ASTROBURST_CACHE_MAX_BYTES` — per-session LRU memory cap in bytes.
    /// Default: `2147483648` (2 GiB).
    pub cache_max_bytes: usize,

    /// `ASTROBURST_CLEANUP_INTERVAL` — seconds between TTL sweep runs.
    /// Default: `60`.
    pub cleanup_interval: Duration,

    /// `ASTROBURST_LOG_LEVEL` — minimum log level (`trace`/`debug`/`info`/`warn`/`error`).
    /// Default: `info`.  Overridden by `RUST_LOG` if that var is also set.
    pub log_level: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8080".parse().expect("default bind is valid"),
            session_ttl: Duration::from_secs(900),
            session_max: 8,
            jobs_max: 4,
            cache_max_entries: 32,
            cache_max_bytes: 2 * 1024 * 1024 * 1024, // 2 GiB
            cleanup_interval: Duration::from_secs(60),
            log_level: "info".to_owned(),
        }
    }
}

impl ServerConfig {
    /// Build a config by reading `ASTROBURST_*` environment variables.
    /// Missing vars use the `Default` values; invalid values warn and also
    /// fall back to the default.
    pub fn from_env() -> Self {
        let d = Self::default();
        Self {
            bind: env_or("ASTROBURST_BIND", d.bind),
            session_ttl: Duration::from_secs(env_or::<u64>(
                "ASTROBURST_SESSION_TTL",
                d.session_ttl.as_secs(),
            )),
            session_max: env_or("ASTROBURST_SESSION_MAX", d.session_max),
            jobs_max: env_or("ASTROBURST_JOBS_MAX", d.jobs_max),
            cache_max_entries: env_or("ASTROBURST_CACHE_MAX_ENTRIES", d.cache_max_entries),
            cache_max_bytes: env_or("ASTROBURST_CACHE_MAX_BYTES", d.cache_max_bytes),
            cleanup_interval: Duration::from_secs(env_or::<u64>(
                "ASTROBURST_CLEANUP_INTERVAL",
                d.cleanup_interval.as_secs(),
            )),
            log_level: env_or("ASTROBURST_LOG_LEVEL", d.log_level),
        }
    }
}

/// Read an env var and parse it into `T`.  On parse failure (or if the var is
/// absent), emit a warning and return `default`.
fn env_or<T>(var: &str, default: T) -> T
where
    T: FromStr + std::fmt::Debug,
    T::Err: std::fmt::Debug,
{
    match std::env::var(var) {
        Err(_) => default, // not set — silent, use default
        Ok(raw) => raw.parse().unwrap_or_else(|e| {
            eprintln!(
                "WARN: {var}={raw:?} is invalid ({e:?}); using default {default:?}"
            );
            default
        }),
    }
}
