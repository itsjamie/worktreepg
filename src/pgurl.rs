//! Text-level handling of `postgres://` URLs. Only the path component (the database name) is
//! ever changed, so credentials, hosts, unix-socket `?host=` parameters, and `sslmode` all
//! pass through exactly as the user wrote them.

use anyhow::{anyhow, Result};
use postgres::config::Host;
use regex::Regex;
use std::str::FromStr;
use std::sync::LazyLock;

static URL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(postgres(?:ql)?://[^/?#]*)(/[^?#]*)?([?#].*)?$").unwrap());
static PASSWORD_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^([a-z]+://[^:/?#@]*):[^@/?#]*@").unwrap());

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgUrl {
    /// The connection string exactly as written in the env file.
    pub raw: String,
    pub database: String,
    /// `user@host:port`; identifies a cluster so several variables pointing at one share a connection.
    pub server_key: String,
    host: String,
}

impl PgUrl {
    /// Accepts libpq-style URLs only. `key=value` strings are rejected because they cannot be
    /// rewritten without reformatting the user's file.
    pub fn parse(value: &str) -> Result<Self> {
        let raw = value.trim().to_string();
        if !URL_RE.is_match(&raw) {
            return Err(anyhow!("not a postgres:// URL: {}", redact(&raw)));
        }
        let config = postgres::Config::from_str(&raw).map_err(|e| anyhow!("{}: {e}", redact(&raw)))?;
        let user = config.get_user().map(str::to_string);
        let database = config
            .get_dbname()
            .map(str::to_string)
            .or_else(|| user.clone())
            .ok_or_else(|| anyhow!("cannot determine the database name from {}", redact(&raw)))?;
        let host = config
            .get_hosts()
            .first()
            .map(|h| match h {
                Host::Tcp(name) => name.clone(),
                #[cfg(unix)]
                Host::Unix(path) => path.display().to_string(),
            })
            .unwrap_or_default();
        let port = config.get_ports().first().map(|p| p.to_string()).unwrap_or_default();
        let server_key = format!("{}@{host}:{port}", user.unwrap_or_default());
        Ok(Self { raw, database, server_key, host })
    }

    /// Loopback or a unix socket, so the server's files may be on this host.
    pub fn is_local(&self) -> bool {
        matches!(self.host.as_str(), "" | "localhost" | "127.0.0.1" | "::1") || self.host.starts_with('/')
    }

    /// The same URL pointing at `database`.
    pub fn with_database(&self, database: &str) -> String {
        with_database(&self.raw, database)
    }

    /// Identifies one database on one cluster.
    pub fn database_key(&self) -> String {
        format!("{}/{}", self.server_key, self.database)
    }

    /// Host description without credentials, for output.
    pub fn server(&self) -> String {
        self.server_key.trim_start_matches('@').trim_end_matches(':').to_string()
    }
}

pub fn with_database(url: &str, database: &str) -> String {
    let Some(caps) = URL_RE.captures(url.trim()) else { return url.to_string() };
    let authority = caps.get(1).map_or("", |m| m.as_str());
    let tail = caps.get(3).map_or("", |m| m.as_str());
    format!("{authority}/{database}{tail}")
}

/// Hides the password so a connection string can appear in diagnostics.
pub fn redact(url: &str) -> String {
    PASSWORD_RE.replace(url, "$1:***@").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_database_and_server_key() {
        let u = PgUrl::parse("postgresql://app:s3cret@db.local:5433/app?sslmode=disable").unwrap();
        assert_eq!(u.database, "app");
        assert_eq!(u.server_key, "app@db.local:5433");
        assert_eq!(u.server(), "app@db.local:5433");
        assert!(!u.is_local());
        assert!(PgUrl::parse("postgres://u@localhost/db").unwrap().is_local());
        assert!(PgUrl::parse("postgres://u@/db?host=/var/run/postgresql").unwrap().is_local());
    }

    #[test]
    fn database_defaults_to_user() {
        assert_eq!(PgUrl::parse("postgres://alice@localhost").unwrap().database, "alice");
    }

    #[test]
    fn rejects_non_urls() {
        assert!(PgUrl::parse("host=localhost dbname=app").is_err());
        assert!(PgUrl::parse("mysql://x/y").is_err());
    }

    #[test]
    fn with_database_swaps_only_the_path() {
        assert_eq!(with_database("postgres://u:p@h:1/app?sslmode=require#x", "app_fork"), "postgres://u:p@h:1/app_fork?sslmode=require#x");
        assert_eq!(
            with_database("postgres://u@/app?host=/var/run/postgresql", "app_fork"),
            "postgres://u@/app_fork?host=/var/run/postgresql"
        );
        assert_eq!(with_database("postgresql://localhost", "x"), "postgresql://localhost/x");
    }

    #[test]
    fn redact_hides_the_password() {
        assert_eq!(redact("postgres://u:p%40ss@h/db"), "postgres://u:***@h/db");
        assert_eq!(redact("postgres://u@h/db"), "postgres://u@h/db");
    }
}
