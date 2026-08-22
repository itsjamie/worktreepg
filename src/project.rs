//! What a repository has configured: where the source and target worktrees are, which
//! `.worktreeinclude` directives apply, and which Postgres databases those directives name.

use crate::directives::{parse_directives, Directive};
use crate::envfile::EnvFile;
use crate::errors::{environment, usage};
use crate::git;
use crate::pgurl::PgUrl;
use anyhow::Result;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_INCLUDE: &str = ".worktreeinclude";

pub struct Project {
    pub cwd: PathBuf,
    /// Root of the source worktree, where `.worktreeinclude` and the live env files live.
    pub source: PathBuf,
    /// Root of the worktree `cwd` is in.
    pub target: PathBuf,
    /// Canonical common `.git` directory; identifies the repository in database metadata.
    pub repo: PathBuf,
    pub include_file: PathBuf,
    directives: Vec<Directive>,
}

/// One directive variable as found in the source worktree.
#[derive(Debug, Clone)]
pub struct EnvVar {
    pub file: String,
    pub name: String,
    pub url: PgUrl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProblemKind {
    MissingFile,
    MissingVar,
    InvalidUrl,
}

/// A directive entry that could not be resolved in the source worktree.
#[derive(Debug, Clone)]
pub struct Problem {
    pub file: String,
    pub name: Option<String>,
    pub kind: ProblemKind,
    pub detail: String,
}

impl Project {
    pub fn load(cwd: &Path, from: &str, include: &str) -> Result<Self> {
        let target = git::worktree_root(cwd)?;
        let source = git::resolve_source(from, cwd)?;
        let repo = git::common_dir(cwd)?;
        if git::common_dir(&source)? != repo {
            return Err(usage("source and target are not from the same repository"));
        }

        let include_path = Path::new(include);
        let include_file = if include_path.is_absolute() { git::canonical(include_path) } else { source.join(include_path) };
        if !include_file.starts_with(&source) {
            return Err(usage(format!("--include must point inside the source worktree ({})", source.display())));
        }

        let directives = match fs::read_to_string(&include_file) {
            Ok(content) => parse_directives(&content)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self { cwd: cwd.to_path_buf(), source, target, repo, include_file, directives })
    }

    pub fn require_directives(&self) -> Result<()> {
        if self.directives.is_empty() {
            let shown = self.include_file.strip_prefix(&self.cwd).unwrap_or(&self.include_file);
            return Err(environment(format!("no \"# worktreepg\" directive found in {}", shown.display())));
        }
        Ok(())
    }

    /// Every directive variable, read from the source worktree's env files.
    pub fn env_vars(&self) -> Result<(Vec<EnvVar>, Vec<Problem>)> {
        let mut vars = Vec::new();
        let mut problems = Vec::new();
        for directive in &self.directives {
            let env = match EnvFile::open(&self.source.join(&directive.file)) {
                Ok(env) => env,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    problems.push(Problem {
                        file: directive.file.clone(),
                        name: None,
                        kind: ProblemKind::MissingFile,
                        detail: format!("{} does not exist in {}", directive.file, self.source.display()),
                    });
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            for name in &directive.vars {
                let Some(value) = env.get(name) else {
                    problems.push(Problem {
                        file: directive.file.clone(),
                        name: Some(name.clone()),
                        kind: ProblemKind::MissingVar,
                        detail: format!("{name} is not set in {}", directive.file),
                    });
                    continue;
                };
                match PgUrl::parse(&value) {
                    Ok(url) => vars.push(EnvVar { file: directive.file.clone(), name: name.clone(), url }),
                    Err(e) => problems.push(Problem {
                        file: directive.file.clone(),
                        name: Some(name.clone()),
                        kind: ProblemKind::InvalidUrl,
                        detail: format!("{}: {name}: {e}", directive.file),
                    }),
                }
            }
        }
        Ok((vars, problems))
    }

    /// The distinct live databases named by the directives, one per (cluster, database name).
    pub fn databases(&self) -> Result<(Vec<PgUrl>, Vec<Problem>)> {
        let (vars, problems) = self.env_vars()?;
        let mut seen = HashSet::new();
        let databases = vars.into_iter().filter(|v| seen.insert(v.url.database_key())).map(|v| v.url).collect();
        Ok((databases, problems))
    }
}
