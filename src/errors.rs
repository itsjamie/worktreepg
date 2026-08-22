//! Exit codes mirror git-worktreeinclude so the two tools can be scripted the same way:
//! 0 success, 1 internal error, 2 usage error, 3 conflict, 4 environment/prerequisite error.

use std::fmt;

pub const EXIT_INTERNAL: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_CONFLICT: i32 = 3;
pub const EXIT_ENVIRONMENT: i32 = 4;

#[derive(Debug)]
pub struct CliError {
    pub code: i32,
    pub message: String,
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

pub fn usage(message: impl Into<String>) -> anyhow::Error {
    CliError { code: EXIT_USAGE, message: message.into() }.into()
}

pub fn conflict(message: impl Into<String>) -> anyhow::Error {
    CliError { code: EXIT_CONFLICT, message: message.into() }.into()
}

pub fn environment(message: impl Into<String>) -> anyhow::Error {
    CliError { code: EXIT_ENVIRONMENT, message: message.into() }.into()
}

pub fn is_conflict(err: &anyhow::Error) -> bool {
    err.downcast_ref::<CliError>().is_some_and(|e| e.code == EXIT_CONFLICT)
}

pub fn exit_code(err: &anyhow::Error) -> i32 {
    err.downcast_ref::<CliError>().map_or(EXIT_INTERNAL, |e| e.code)
}
