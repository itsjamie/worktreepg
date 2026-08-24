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
    /// `host:port`; identifies a cluster, so two variables naming one database are one database
    /// whichever role each connects as.
    pub cluster_key: String,
    /// The role this URL connects as.
    pub user: String,
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
        let cluster_key = format!("{host}:{port}");
        Ok(Self { raw, database, cluster_key, user: user.unwrap_or_default(), host })
    }

    /// Loopback or a unix socket, so the server's files may be on this host.
    pub fn is_local(&self) -> bool {
        matches!(self.host.as_str(), "" | "localhost" | "127.0.0.1" | "::1") || self.host.starts_with('/')
    }

    /// The same URL pointing at `database`.
    pub fn with_database(&self, database: &str) -> String {
        with_database(&self.raw, database)
    }

    /// Identifies one physical database on one cluster, whatever credentials name it.
    pub fn database_key(&self) -> String {
        self.database_key_of(&self.database)
    }

    /// The same key for another database on this URL's cluster: a fork's source, which a scan
    /// turned up rather than a directive, so this URL need not name it itself.
    pub fn database_key_of(&self, database: &str) -> String {
        format!("{}/{database}", self.cluster_key)
    }

    /// Identifies one admin connection: a cluster reached as one role.
    pub fn role_key(&self) -> String {
        format!("{}@{}", self.user, self.cluster_key)
    }

    /// The cluster as a person names it, without credentials, for output.
    pub fn server(&self) -> String {
        self.cluster_key.trim_end_matches(':').to_string()
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
    fn parses_database_and_cluster_key() {
        let u = PgUrl::parse("postgresql://app:s3cret@db.local:5433/app?sslmode=disable").unwrap();
        assert_eq!(u.database, "app");
        assert_eq!(u.user, "app");
        assert_eq!(u.cluster_key, "db.local:5433");
        assert_eq!(u.server(), "db.local:5433");
        assert!(!u.is_local());
        assert!(PgUrl::parse("postgres://u@localhost/db").unwrap().is_local());
        assert!(PgUrl::parse("postgres://u@/db?host=/var/run/postgresql").unwrap().is_local());
        assert_eq!(PgUrl::parse("postgres://u@/db?host=/var/run/postgresql").unwrap().server(), "/var/run/postgresql");
    }

    #[test]
    fn credentials_do_not_split_a_cluster_or_a_database() {
        let owner = PgUrl::parse("postgres://postgres:pw@127.0.0.1:5432/app").unwrap();
        let runtime = PgUrl::parse("postgres://app_user:other@127.0.0.1:5432/app").unwrap();
        let audit = PgUrl::parse("postgres://app_user:other@127.0.0.1:5432/audit").unwrap();
        assert_eq!(owner.cluster_key, runtime.cluster_key);
        assert_eq!(owner.database_key(), runtime.database_key());
        assert_eq!(audit.cluster_key, owner.cluster_key);
        assert_ne!(audit.database_key(), owner.database_key());
        assert_ne!(owner.role_key(), runtime.role_key());
        assert_eq!(runtime.role_key(), audit.role_key());
        // a database on the cluster that this URL does not name itself
        assert_eq!(owner.database_key_of("audit"), audit.database_key());
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
