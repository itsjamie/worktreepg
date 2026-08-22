//! Everything worktreepg needs to know from git: which worktrees exist, which one a path is
//! in, which repository they share, and what to call a worktree.

use crate::errors::environment;
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    /// Full ref such as `refs/heads/main`; `None` when detached or bare.
    pub branch: Option<String>,
    pub bare: bool,
}

pub fn git(args: &[&str], cwd: &Path) -> Result<String> {
    let output = Command::new("git").args(args).current_dir(cwd).output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            environment("git is not installed or not on PATH")
        } else {
            environment(format!("cannot run git: {e}"))
        }
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.to_lowercase().contains("not a git repository") {
            return Err(environment("not inside a git repository"));
        }
        return Err(environment(format!("git {} failed: {stderr}", args.join(" "))));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parses `git worktree list --porcelain -z` output.
pub fn parse_worktree_list(output: &str) -> Vec<Worktree> {
    let mut worktrees = Vec::new();
    for record in output.split("\0\0") {
        let mut wt = Worktree { path: PathBuf::new(), branch: None, bare: false };
        for line in record.split('\0').filter(|l| !l.is_empty()) {
            let (key, value) = line.split_once(' ').unwrap_or((line, ""));
            match key {
                "worktree" => wt.path = PathBuf::from(value),
                "branch" => wt.branch = Some(value.to_string()),
                "bare" => wt.bare = true,
                _ => {}
            }
        }
        if wt.path.as_os_str().is_empty() {
            continue;
        }
        worktrees.push(wt);
    }
    worktrees
}

pub fn list_worktrees(cwd: &Path) -> Result<Vec<Worktree>> {
    Ok(parse_worktree_list(&git(&["worktree", "list", "--porcelain", "-z"], cwd)?))
}

/// Canonical paths of every worktree git still knows about.
pub fn living_worktrees(cwd: &Path) -> Result<Vec<PathBuf>> {
    Ok(list_worktrees(cwd)?.iter().map(|wt| canonical(&wt.path)).collect())
}

/// Root of the worktree containing `cwd`.
pub fn worktree_root(cwd: &Path) -> Result<PathBuf> {
    let out = git(&["rev-parse", "--show-toplevel"], cwd)?;
    let out = out.trim();
    if out.is_empty() {
        return Err(environment(format!("{} is inside a bare repository, not a worktree", cwd.display())));
    }
    Ok(canonical(Path::new(out)))
}

/// The shared `.git` directory, which identifies the repository across all of its worktrees.
pub fn common_dir(cwd: &Path) -> Result<PathBuf> {
    let out = git(&["rev-parse", "--path-format=absolute", "--git-common-dir"], cwd)?;
    Ok(canonical(Path::new(out.trim())))
}

/// Short branch name, or `None` on a detached HEAD.
pub fn current_branch(cwd: &Path) -> Result<Option<String>> {
    let out = git(&["rev-parse", "--abbrev-ref", "HEAD"], cwd)?;
    let out = out.trim();
    Ok(if out.is_empty() || out == "HEAD" { None } else { Some(out.to_string()) })
}

/// What a worktree is called for naming purposes: its branch, or its directory name when detached.
pub fn worktree_name(root: &Path) -> Result<String> {
    Ok(current_branch(root)?.unwrap_or_else(|| root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()))
}

/// Mirrors git-worktreeinclude's `--from`: `auto` picks the first non-bare worktree reported by
/// `git worktree list`, which is the main worktree unless the repository is bare.
pub fn resolve_source(from: &str, cwd: &Path) -> Result<PathBuf> {
    if from != "auto" {
        return worktree_root(&cwd.join(from));
    }
    list_worktrees(cwd)?
        .into_iter()
        .find(|wt| !wt.bare)
        .map(|wt| canonical(&wt.path))
        .ok_or_else(|| environment("no non-bare worktree found to use as the source"))
}

pub fn remove_worktree(path: &Path, force: bool, cwd: &Path) -> Result<()> {
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    let p = path.to_string_lossy().into_owned();
    args.push(&p);
    git(&args, cwd).map(drop)
}

/// Resolves symlinks when the path exists; otherwise just makes it absolute.
pub fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir().map(|d| d.join(path)).unwrap_or_else(|_| path.to_path_buf())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain_z_output() {
        let out = "worktree /repo\0HEAD aaa\0branch refs/heads/main\0\0worktree /repo-wt\0HEAD bbb\0branch refs/heads/feature/x\0locked\0\0worktree /repo-detached\0HEAD ccc\0detached\0prunable gone\0\0worktree /bare.git\0bare\0\0";
        assert_eq!(
            parse_worktree_list(out),
            vec![
                Worktree { path: "/repo".into(), branch: Some("refs/heads/main".into()), bare: false },
                Worktree { path: "/repo-wt".into(), branch: Some("refs/heads/feature/x".into()), bare: false },
                Worktree { path: "/repo-detached".into(), branch: None, bare: false },
                Worktree { path: "/bare.git".into(), branch: None, bare: true },
            ]
        );
    }
}
