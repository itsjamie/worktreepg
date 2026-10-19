//! `.worktreeinclude` is gitignore-compatible, so anything after `#` is a comment that
//! git-worktreeinclude ignores. worktreepg reads directives out of those comments:
//!
//! ```text
//! # worktreepg: .env DATABASE_URL
//! # worktreepg: apps/api/.env DATABASE_URL DIRECT_URL
//! # worktreepg: DATABASE_URL          (file defaults to .env)
//! # worktreepg: .env.local            (variable defaults to DATABASE_URL)
//! # worktreepg                        (both defaults)
//! # worktreepg: .env DATABASE_URL share=shop
//! ```
//!
//! The first token is the env file, relative to the worktree root. Every token after it is
//! a variable in that file whose Postgres connection string should be pointed at the fork.
//!
//! A `share=<group>` token says the forks of the databases this directive names may be shared
//! with other repositories whose directives name the same group (see [`Share`]). It is taken out
//! of the line before the positional grammar applies, so it can go anywhere and every form above
//! takes one; a token starting `share=` is therefore never an env file or a variable.

use crate::errors::environment;
use anyhow::Result;
use regex::Regex;
use std::path::{Component, Path};
use std::sync::LazyLock;

pub const DEFAULT_ENV_FILE: &str = ".env";
pub const DEFAULT_VAR: &str = "DATABASE_URL";

/// Whether a directive's forks may belong to more than one repository.
///
/// A fork name is `<database>_<worktree name>`, so two repositories that point at one development
/// database and run `apply --worktree-name` with one name (one workerbox worker, say) ask for the
/// same fork. Without a group that is a collision and refused, as a fork of another repository
/// always was. With one, the second repository joins the fork the first made, the way both apps
/// already share the development database. Both sides have to say so: a fork made without a group
/// is never joined, and a group only admits directives naming the same one, so a repository
/// cannot take over, or later drop, a fork it was not invited into.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Share {
    /// The fork belongs to one worktree of this repository, as every fork did before groups.
    #[default]
    Private,
    /// The fork may belong to one worktree in each repository whose directive names this group.
    Group(String),
}

impl Share {
    pub fn group(&self) -> Option<&str> {
        match self {
            Share::Private => None,
            Share::Group(group) => Some(group),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Directive {
    /// Env file path, relative to the worktree root, slash-separated.
    pub file: String,
    /// Variables inside `file` holding Postgres connection strings.
    pub vars: Vec<String>,
    pub share: Share,
    /// 1-based line number in `.worktreeinclude`, for diagnostics.
    pub line: usize,
}

static DIRECTIVE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*#\s*worktreepg(?P<rest>(?:[\s:].*)?)$").unwrap());
static VAR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$").unwrap());
/// Lowercase only, so `Shop` and `shop` cannot be two groups that look like one, and short enough
/// to sit in a database comment next to the paths it is compared against.
static GROUP_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z0-9][a-z0-9_.-]{0,62}$").unwrap());
const SHARE_PREFIX: &str = "share=";

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
        let (share, tokens) = share_of(rest.trim_start_matches([':', ' ', '\t']).split_whitespace(), line)?;

        match tokens.as_slice() {
            [] => directives.push(Directive { file: DEFAULT_ENV_FILE.into(), vars: vec![DEFAULT_VAR.into()], share, line }),
            [single] if looks_like_var(single) => {
                directives.push(Directive { file: DEFAULT_ENV_FILE.into(), vars: vec![(*single).into()], share, line });
            }
            [file, vars @ ..] => {
                if Path::new(file).components().any(|part| !matches!(part, Component::CurDir | Component::Normal(_))) {
                    return Err(environment(format!(".worktreeinclude:{line}: env file must be relative to the worktree: \"{file}\"")));
                }
                let vars: Vec<String> =
                    if vars.is_empty() { vec![DEFAULT_VAR.into()] } else { vars.iter().map(|v| (*v).to_string()).collect() };
                for v in &vars {
                    if !VAR_RE.is_match(v) {
                        return Err(environment(format!(".worktreeinclude:{line}: \"{v}\" is not a valid environment variable name")));
                    }
                }
                directives.push(Directive { file: file.trim_start_matches("./").to_string(), vars, share, line });
            }
        }
    }
    Ok(directives)
}

/// Takes the `share=` token out of a directive's tokens, leaving the positional ones in order.
/// One per line: a second, even naming the same group, is more likely a paste than a meaning.
fn share_of<'a>(tokens: impl Iterator<Item = &'a str>, line: usize) -> Result<(Share, Vec<&'a str>)> {
    let mut share = Share::Private;
    let mut rest = Vec::new();
    for token in tokens {
        let Some(group) = token.strip_prefix(SHARE_PREFIX) else {
            rest.push(token);
            continue;
        };
        if share != Share::Private {
            return Err(environment(format!(".worktreeinclude:{line}: a directive takes one share= token")));
        }
        if !GROUP_RE.is_match(group) {
            return Err(environment(format!(
                ".worktreeinclude:{line}: \"{group}\" is not a share group; use lowercase letters, digits, '.', '_' and '-', starting with a letter or digit"
            )));
        }
        share = Share::Group(group.to_string());
    }
    Ok((share, rest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::EXIT_ENVIRONMENT;

    fn d(file: &str, vars: &[&str], line: usize) -> Directive {
        Directive { file: file.into(), vars: vars.iter().map(|v| (*v).into()).collect(), share: Share::Private, line }
    }

    fn shared(file: &str, vars: &[&str], group: &str, line: usize) -> Directive {
        Directive { share: Share::Group(group.into()), ..d(file, vars, line) }
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
        let err = parse_directives("# worktreepg: .env DATABASE-URL").unwrap_err();
        assert_eq!(crate::errors::exit_code(&err), EXIT_ENVIRONMENT);
    }

    #[test]
    fn rejects_env_files_outside_the_worktree() {
        for file in ["../.env", "/tmp/.env"] {
            let err = parse_directives(&format!("# worktreepg: {file} DATABASE_URL")).unwrap_err();
            assert_eq!(crate::errors::exit_code(&err), EXIT_ENVIRONMENT);
        }
    }

    #[test]
    fn a_share_token_fits_every_form_without_becoming_a_file_or_a_variable() {
        let content = "# worktreepg: .env DATABASE_URL share=shop\n# worktreepg: share=shop apps/api/.env DATABASE_URL DIRECT_URL\n# worktreepg DATABASE_URL share=shop\n# worktreepg: .env.local share=shop\n# worktreepg share=shop\n# worktreepg: .env share=shop.v2-a_b\n";
        assert_eq!(
            parse_directives(content).unwrap(),
            vec![
                shared(".env", &["DATABASE_URL"], "shop", 1),
                shared("apps/api/.env", &["DATABASE_URL", "DIRECT_URL"], "shop", 2),
                shared(".env", &["DATABASE_URL"], "shop", 3),
                shared(".env.local", &["DATABASE_URL"], "shop", 4),
                shared(".env", &["DATABASE_URL"], "shop", 5),
                shared(".env", &["DATABASE_URL"], "shop.v2-a_b", 6),
            ]
        );
    }

    #[test]
    fn rejects_malformed_share_groups() {
        for line in [
            "# worktreepg: .env DATABASE_URL share=",
            "# worktreepg: .env DATABASE_URL share=Shop",
            "# worktreepg: .env DATABASE_URL share=-shop",
            "# worktreepg: .env DATABASE_URL share=a/b",
            "# worktreepg: .env DATABASE_URL share=shop share=shop",
            "# worktreepg: .env DATABASE_URL share=shop share=web",
        ] {
            let err = parse_directives(line).unwrap_err();
            assert_eq!(crate::errors::exit_code(&err), EXIT_ENVIRONMENT, "{line}");
        }
        assert!(parse_directives(&format!("# worktreepg share={}", "a".repeat(64))).is_err());
        assert!(parse_directives(&format!("# worktreepg share={}", "a".repeat(63))).is_ok());
    }
}
