//! Configuration for hive-board, read from the environment.
//!
//! Errors carry variable NAMES only, never values: the database URL and the API
//! tokens must never reach a log line or a response.

use nuvek_web::config::optional_env;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("required environment variable `{0}` is not set")]
    Missing(&'static str),
    #[error(
        "`HIVE_API_TOKEN` must be at least 32 characters (or set HIVE_AUTH=off for local dev)"
    )]
    TokenTooShort,
    #[error("`HIVE_API_TOKEN_NEXT` must be at least 32 characters when set")]
    NextTokenTooShort,
}

/// Lifecycle thresholds.
#[derive(Debug, Clone, Copy)]
pub struct Lifecycle {
    pub quiet_after_h: i64,
    pub resolve_after_h: i64,
    pub archive_after_h: i64,
    pub compact_after_d: i64,
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self {
            quiet_after_h: 12,
            resolve_after_h: 72,
            archive_after_h: 24,
            compact_after_d: 30,
        }
    }
}

/// Jobs settings.
#[derive(Debug, Clone)]
pub struct JobsConfig {
    /// `HIVE_JOB_OWNS_ROOTS`: every `owns` entry must lie strictly below one of these.
    ///
    /// Empty by default, and empty fails closed: no `owns` entry is accepted, so a job that
    /// declares one (every `local-build` job must) is refused until the operator names the
    /// roots agents may claim work under.
    pub owns_roots: Vec<String>,
    /// `HIVE_JOB_STALE_H`: a claimed job older than this, with no topic activity for an hour, is stale.
    pub stale_h: i64,
}

impl Default for JobsConfig {
    fn default() -> Self {
        Self {
            owns_roots: Vec::new(),
            stale_h: 6,
        }
    }
}

/// Parse `HIVE_JOB_OWNS_ROOTS`: a comma list of absolute paths. Relative entries and `/` itself
/// are dropped (a root of `/` would make every path ownable).
pub fn parse_owns_roots(v: &str) -> Vec<String> {
    v.split(',')
        .map(|r| r.trim().trim_end_matches('/').to_string())
        .filter(|r| r.starts_with('/') && r.len() > 1)
        .collect()
}

impl JobsConfig {
    pub fn from_env() -> Self {
        Self::from_lookup(&|k| optional_env(k))
    }

    /// Like [`JobsConfig::from_env`], reading variables through `get`.
    pub fn from_lookup(get: &dyn Fn(&str) -> Option<String>) -> Self {
        let d = Self::default();
        let owns_roots = get("HIVE_JOB_OWNS_ROOTS")
            .map(|v| parse_owns_roots(&v))
            .unwrap_or(d.owns_roots);
        Self {
            owns_roots,
            stale_h: int_var(get, "HIVE_JOB_STALE_H", d.stale_h).clamp(1, 8760),
        }
    }
}

/// Bearer tokens accepted on every write, and on reads while the read gate is on. Never
/// printed: no `Debug` derive.
#[derive(Clone, Default)]
pub struct AuthConfig {
    pub off: bool,
    pub token: Option<String>,
    pub token_next: Option<String>,
    /// `HIVE_AUTH_READS`: GET routes need the token too (except `/readyz`, which the
    /// container health check calls without one). Default on; `HIVE_AUTH_READS=0` opens reads
    /// to anyone who can reach the port.
    pub reads: bool,
}

/// Default listen address: loopback only. `BIND=0.0.0.0` opts into every interface (the
/// container image sets it, because a container is reached through its published port).
pub const DEFAULT_BIND: &str = "127.0.0.1";

/// The read gate fails closed: unset, blank or unrecognised keeps reads behind the token, and
/// only an explicit `0`, `false`, `no` or `off` opens them.
pub fn reads_gated(v: Option<&str>) -> bool {
    !matches!(
        v.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("0" | "false" | "no" | "off")
    )
}

pub struct Config {
    pub bind: String,
    pub port: u16,
    pub database_url: String,
    pub auth: AuthConfig,
    pub db_max_connections: u32,
    pub lifecycle: Lifecycle,
    pub jobs: JobsConfig,
}

fn int_var(get: &dyn Fn(&str) -> Option<String>, key: &str, default: i64) -> i64 {
    get(key)
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

fn truthy(v: Option<String>) -> bool {
    matches!(
        v.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

fn parse_port(v: Option<String>) -> u16 {
    v.and_then(|p| p.trim().parse().ok()).unwrap_or(8080)
}

/// `PORT`, parsed identically by the server and the `healthcheck` subcommand.
pub fn port_from_env() -> u16 {
    parse_port(optional_env("PORT"))
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(&|k| optional_env(k))
    }

    /// Like [`Config::from_env`], reading variables through `get` (tests pass a map).
    pub fn from_lookup(get: &dyn Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let database_url = get("HIVE_DATABASE_URL")
            .filter(|v| !v.trim().is_empty())
            .ok_or(ConfigError::Missing("HIVE_DATABASE_URL"))?;

        let off = truthy(get("HIVE_AUTH_OFF"))
            || get("HIVE_AUTH").is_some_and(|v| v.trim().eq_ignore_ascii_case("off"));
        let token = get("HIVE_API_TOKEN").filter(|v| !v.trim().is_empty());
        let token_next = get("HIVE_API_TOKEN_NEXT").filter(|v| !v.trim().is_empty());
        if !off {
            match &token {
                None => return Err(ConfigError::Missing("HIVE_API_TOKEN")),
                Some(t) if t.chars().count() < 32 => return Err(ConfigError::TokenTooShort),
                Some(_) => {}
            }
            if token_next.as_ref().is_some_and(|t| t.chars().count() < 32) {
                return Err(ConfigError::NextTokenTooShort);
            }
        }

        let d = Lifecycle::default();
        Ok(Self {
            bind: get("BIND").unwrap_or_else(|| DEFAULT_BIND.to_string()),
            port: parse_port(get("PORT")),
            database_url,
            auth: AuthConfig {
                off,
                token,
                token_next,
                reads: reads_gated(get("HIVE_AUTH_READS").as_deref()),
            },
            db_max_connections: int_var(get, "HIVE_DB_MAX_CONNECTIONS", 6).clamp(1, 64) as u32,
            jobs: JobsConfig::from_lookup(get),
            lifecycle: Lifecycle {
                // Clamped so make_interval(hours/days => int) can never overflow.
                quiet_after_h: int_var(get, "HIVE_QUIET_AFTER_H", d.quiet_after_h).clamp(1, 8760),
                resolve_after_h: int_var(get, "HIVE_RESOLVE_AFTER_H", d.resolve_after_h)
                    .clamp(1, 8760),
                archive_after_h: int_var(get, "HIVE_ARCHIVE_AFTER_H", d.archive_after_h)
                    .clamp(1, 8760),
                compact_after_d: int_var(get, "HIVE_COMPACT_AFTER_D", d.compact_after_d)
                    .clamp(1, 3650),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    const MINIMAL: [(&str, &str); 2] = [
        ("HIVE_DATABASE_URL", "postgres://localhost/hive"),
        ("HIVE_API_TOKEN", "0123456789abcdef0123456789abcdef"), // gitleaks:allow (test token)
    ];

    #[test]
    fn defaults_are_loopback_gated_reads_and_no_owns_roots() {
        let c = Config::from_lookup(&lookup(&MINIMAL)).expect("config");
        assert_eq!(c.bind, "127.0.0.1");
        assert_eq!(c.port, 8080);
        assert!(c.auth.reads, "reads must be gated by default");
        assert!(!c.auth.off);
        assert!(c.jobs.owns_roots.is_empty());
    }

    #[test]
    fn deployment_overrides_still_apply() {
        let mut pairs = MINIMAL.to_vec();
        pairs.extend([
            ("BIND", "0.0.0.0"),
            ("HIVE_AUTH_READS", "0"),
            ("HIVE_JOB_OWNS_ROOTS", "/srv/repos, /opt/work/"),
        ]);
        let c = Config::from_lookup(&lookup(&pairs)).expect("config");
        assert_eq!(c.bind, "0.0.0.0");
        assert!(!c.auth.reads);
        assert_eq!(c.jobs.owns_roots, vec!["/srv/repos", "/opt/work"]);
    }

    #[test]
    fn read_gate_fails_closed() {
        for v in [
            None,
            Some(""),
            Some(" "),
            Some("1"),
            Some("ture"),
            Some("yes"),
        ] {
            assert!(reads_gated(v), "{v:?}");
        }
        for v in ["0", "false", "NO", " off "] {
            assert!(!reads_gated(Some(v)), "{v}");
        }
    }

    #[test]
    fn owns_roots_drop_relative_and_bare_slash() {
        assert!(parse_owns_roots("relative, /, ").is_empty());
        assert_eq!(parse_owns_roots("/a/b/,x,/c"), vec!["/a/b", "/c"]);
    }

    #[test]
    fn token_is_required_unless_auth_is_off() {
        let only_db = [("HIVE_DATABASE_URL", "postgres://localhost/hive")];
        assert!(matches!(
            Config::from_lookup(&lookup(&only_db)),
            Err(ConfigError::Missing("HIVE_API_TOKEN"))
        ));
        let off = [
            ("HIVE_DATABASE_URL", "postgres://localhost/hive"),
            ("HIVE_AUTH", "off"),
        ];
        assert!(Config::from_lookup(&lookup(&off)).expect("config").auth.off);
    }
}
