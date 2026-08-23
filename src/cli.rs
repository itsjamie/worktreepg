//! Command-line surface. Clap handles usage errors (exit 2) and `--help`/`--version`.

use crate::commands::{self, ApplyOptions, RemoveOptions, TemplateAction, TemplateCommand};
use crate::errors::exit_code;
use crate::project::{Project, DEFAULT_INCLUDE};
use crate::report::Reporter;
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::process::ExitCode;

/// Fork a Postgres database per git worktree, driven by a "# worktreepg" comment in .worktreeinclude.
#[derive(Parser)]
#[command(name = "git worktreepg", bin_name = "git worktreepg", version, about, after_help = AFTER_HELP)]
struct Cli {
    /// Source worktree: "auto" picks the first non-bare worktree, as git-worktreeinclude does
    #[arg(long, global = true, default_value = "auto", value_name = "auto|path")]
    from: String,
    /// .worktreeinclude location, relative to the source worktree
    #[arg(long, global = true, default_value = DEFAULT_INCLUDE, value_name = "path")]
    include: String,
    /// Emit a single JSON object on stdout
    #[arg(long, global = true)]
    json: bool,
    /// Suppress human-readable output
    #[arg(long, global = true)]
    quiet: bool,
    /// Print additional details
    #[arg(long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    command: Command,
}

const AFTER_HELP: &str = "Directive, inside .worktreeinclude:
  # worktreepg: .env DATABASE_URL
  # worktreepg: apps/api/.env DATABASE_URL DIRECT_URL

Exit codes: 0 success, 1 internal error, 2 usage error, 3 conflict, 4 environment error.";

#[derive(Args)]
struct Common {
    /// Plan only, change nothing
    #[arg(long)]
    dry_run: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Fork the database for this worktree and point its env file at the fork
    Apply {
        /// Name to derive the database name from (default: the branch, or the directory when detached)
        #[arg(long, value_name = "name")]
        worktree_name: Option<String>,
        /// Drop the existing fork and clone it again from current data
        #[arg(long)]
        recreate: bool,
        /// Close open connections to the live database and clone it, rather than falling back to the template
        #[arg(long)]
        terminate: bool,
        /// Rewrite the env variable even if it points at some other database
        #[arg(long)]
        force: bool,
        #[command(flatten)]
        common: Common,
    },
    /// Remove a worktree (git worktree remove) and drop its forks
    Remove {
        /// Worktree path (default: the current one)
        path: Option<String>,
        /// Drop the databases but leave the worktree in place
        #[arg(long)]
        keep_worktree: bool,
        /// Passed through to git worktree remove
        #[arg(long)]
        force: bool,
        #[command(flatten)]
        common: Common,
    },
    /// Drop forks whose worktree no longer exists
    Prune {
        #[command(flatten)]
        common: Common,
    },
    /// Show the template and forks for this repository
    List {
        /// Include databases created for other repositories on the same cluster
        #[arg(long)]
        all: bool,
    },
    /// Manage <database>_template, the snapshot forks are cloned from while the live database is in use
    Template {
        #[arg(value_enum)]
        action: TemplateArg,
        /// Close open connections to the live database before copying it
        #[arg(long)]
        terminate: bool,
        /// Take over a database of the template's name that worktreepg did not create
        #[arg(long)]
        force: bool,
        #[command(flatten)]
        common: Common,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum TemplateArg {
    /// Create the template if it does not exist
    Create,
    /// Replace the template with a fresh copy of the live database
    Refresh,
    /// Drop the template
    Drop,
}

pub fn run() -> ExitCode {
    let cli = Cli::parse();
    let mut reporter = Reporter::new(cli.json, cli.quiet, cli.verbose);
    let result = std::env::current_dir()
        .map_err(anyhow::Error::from)
        .and_then(|cwd| Project::load(&cwd, &cli.from, &cli.include))
        .and_then(|project| match &cli.command {
            Command::Apply { worktree_name, recreate, terminate, force, common } => commands::apply(
                &project,
                &ApplyOptions {
                    worktree_name: worktree_name.clone(),
                    recreate: *recreate,
                    terminate: *terminate,
                    dry_run: common.dry_run,
                    force: *force,
                },
                &mut reporter,
            ),
            Command::Remove { path, keep_worktree, force, common } => commands::remove(
                &project,
                &RemoveOptions { path: path.clone(), keep_worktree: *keep_worktree, dry_run: common.dry_run, force: *force },
                &mut reporter,
            ),
            Command::Prune { common } => commands::prune(&project, common.dry_run, &mut reporter),
            Command::List { all } => commands::list(&project, *all, &mut reporter),
            Command::Template { action, terminate, force, common } => commands::template(
                &project,
                &TemplateCommand {
                    action: match action {
                        TemplateArg::Create => TemplateAction::Create,
                        TemplateArg::Refresh => TemplateAction::Refresh,
                        TemplateArg::Drop => TemplateAction::Drop,
                    },
                    terminate: *terminate,
                    dry_run: common.dry_run,
                    force: *force,
                },
                &mut reporter,
            ),
        });
    match result {
        Ok(code) => ExitCode::from(code as u8),
        Err(err) => {
            eprintln!("worktreepg: {err:#}");
            ExitCode::from(exit_code(&err) as u8)
        }
    }
}
