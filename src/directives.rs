//! `.worktreeinclude` is gitignore-compatible, so anything after `#` is a comment that
//! git-worktreeinclude ignores. worktreepg reads directives out of those comments:
//!
//! ```text
//! # worktreepg: .env DATABASE_URL
//! # worktreepg: apps/api/.env DATABASE_URL DIRECT_URL
//! # worktreepg: DATABASE_URL          (file defaults to .env)
//! # worktreepg: .env.local            (variable defaults to DATABASE_URL)
//! # worktreepg                        (both defaults)
//! ```
//!
//! The first token is the env file, relative to the worktree root. Every token after it is
//! a variable in that file whose Postgres connection string should be pointed at the fork.

use anyhow::{bail, Result};
use regex::Regex;
use std::sync::LazyLock;

pub const DEFAULT_ENV_FILE: &str = ".env";
pub const DEFAULT_VAR: &str = "DATABASE_URL";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Directive {
    /// Env file path, relative to the worktree root, slash-separated.
    pub file: String,
    /// Variables inside `file` holding Postgres connection strings.
    pub vars: Vec<String>,
    /// 1-based line number in `.worktreeinclude`, for diagnostics.
    pub line: usize,
}

static DIRECTIVE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*#\s*worktreepg(?P<rest>(?:[\s:].*)?)$").unwrap());
static VAR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$").unwrap());

/// A bare SCREAMING_CASE token is a variable; anything with a dot or slash is a path.
fn looks_like_var(token: &str) -> bool {
    VAR_RE.is_match(token) && token == token.to_uppercase()
}

pub fn parse_directives(content: &str) -> Result<Vec<Directive>> {
    let mut directives = Vec::new();
    for (i, raw) in content.lines().enumerate() {
        let Some(caps) = DIRECTIVE_RE.captures(raw) else { continue };
        let line = i + 1;
        let rest = caps.name("rest").map_or("", |m| m.as_str());
        let tokens: Vec<&str> = rest.trim_start_matches([':', ' ', '\t']).split_whitespace().collect();

        match tokens.as_slice() {
            [] => directives.push(Directive { file: DEFAULT_ENV_FILE.into(), vars: vec![DEFAULT_VAR.into()], line }),
            [single] if looks_like_var(single) => {
                directives.push(Directive { file: DEFAULT_ENV_FILE.into(), vars: vec![(*single).into()], line });
            }
            [file, vars @ ..] => {
                let vars: Vec<String> =
                    if vars.is_empty() { vec![DEFAULT_VAR.into()] } else { vars.iter().map(|v| (*v).to_string()).collect() };
                for v in &vars {
                    if !VAR_RE.is_match(v) {
                        bail!(".worktreeinclude:{line}: \"{v}\" is not a valid environment variable name");
                    }
                }
                directives.push(Directive { file: file.trim_start_matches("./").to_string(), vars, line });
            }
        }
    }
    Ok(directives)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(file: &str, vars: &[&str], line: usize) -> Directive {
        Directive { file: file.into(), vars: vars.iter().map(|v| (*v).into()).collect(), line }
    }

    #[test]
    fn parses_the_documented_forms() {
        let content = "# worktreepg: .env DATABASE_URL\n# worktreepg: apps/api/.env DATABASE_URL DIRECT_URL\n#worktreepg DATABASE_URL\n# worktreepg: .env.local\n# worktreepg\n# worktreepg: ./packages/db/.env SHADOW_URL\n.env\n!.env.example\n# a plain comment mentioning worktreepg-like text\n";
        assert_eq!(
            parse_directives(content).unwrap(),
            vec![
                d(".env", &["DATABASE_URL"], 1),
                d("apps/api/.env", &["DATABASE_URL", "DIRECT_URL"], 2),
                d(".env", &["DATABASE_URL"], 3),
                d(".env.local", &["DATABASE_URL"], 4),
                d(".env", &["DATABASE_URL"], 5),
                d("packages/db/.env", &["SHADOW_URL"], 6),
            ]
        );
    }

    #[test]
    fn ignores_patterns_and_unrelated_comments() {
        assert_eq!(parse_directives(".env\n# worktreepg-ish\nworktreepg: .env X\n").unwrap(), vec![]);
    }

    #[test]
    fn rejects_invalid_variable_names() {
        assert!(parse_directives("# worktreepg: .env DATABASE-URL").is_err());
    }
}
