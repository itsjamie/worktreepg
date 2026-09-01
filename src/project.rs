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
use std::path::{Component, Path, PathBuf};

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

        let include_file = contained_path(&source, Path::new(include))
            .ok_or_else(|| usage(format!("--include must point inside the source worktree ({})", source.display())))?;

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
            let path = contained_path(&self.source, Path::new(&directive.file))
                .ok_or_else(|| environment(format!("{} resolves outside the source worktree", directive.file)))?;
            let env = match EnvFile::open(&path) {
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
}

/// Resolves `path` under a canonical worktree root, rejecting traversal and symlink escapes.
pub(crate) fn contained_path(root: &Path, path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() && path.components().any(|part| matches!(part, Component::ParentDir | Component::RootDir | Component::Prefix(_)))
    {
        return None;
    }
    let joined = if path.is_absolute() { path.to_path_buf() } else { root.join(path) };
    let resolved = git::canonical(&joined);
    resolved.starts_with(root).then_some(resolved)
}

/// The distinct live databases the variables name, one per (cluster, database name). Several
/// variables naming one database with different credentials collapse to the first, whose
/// credentials every statement on that database then runs as.
pub fn databases(vars: &[EnvVar]) -> Vec<PgUrl> {
    distinct(vars, PgUrl::database_key)
}

/// One URL per cluster, for the work that spans everything a cluster holds rather than one
/// database at a time. Going by database instead would visit a cluster once per database it
/// holds, and each of those visits sees every fork on it.
pub fn clusters(vars: &[EnvVar]) -> Vec<PgUrl> {
    distinct(vars, |url| url.cluster_key.clone())
}

/// The variables' URLs, keeping the first of each `key`.
fn distinct(vars: &[EnvVar], key: impl Fn(&PgUrl) -> String) -> Vec<PgUrl> {
    let mut seen = HashSet::new();
    vars.iter().filter(|v| seen.insert(key(&v.url))).map(|v| v.url.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_paths_reject_traversal_and_symlink_escapes() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join(".env");
        fs::write(&outside_file, "DATABASE_URL=x\n").unwrap();

        assert!(contained_path(&root, Path::new("../.env")).is_none());
        assert!(contained_path(&root, &outside_file).is_none());
        assert_eq!(contained_path(&root, Path::new("inside.env")), Some(root.join("inside.env")));

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside_file, root.join(".env")).unwrap();
            assert!(contained_path(&root, Path::new(".env")).is_none());
        }
    }
}
