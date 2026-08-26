//! Exit codes mirror git-worktreeinclude so the two tools can be scripted the same way:
//! 0 success, 1 internal error, 2 usage error, 3 conflict, 4 environment/prerequisite error.

use serde_json::Value;
use std::fmt;

pub const EXIT_INTERNAL: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_CONFLICT: i32 = 3;
pub const EXIT_ENVIRONMENT: i32 = 4;

#[derive(Debug)]
pub struct CliError {
    pub code: i32,
    pub message: String,
    /// How the caller reports this error, for the ones that tell several kinds of a single exit
    /// code apart in their machine-readable output. `None` where their generic status is right.
    pub detail: Option<Detail>,
}

/// What one kind of error puts in the action the caller reports it in: the status to use instead
/// of the caller's generic one, and the fields that let a `--json` caller act on the error
/// without reading its message.
#[derive(Debug, Clone)]
pub struct Detail {
    pub status: &'static str,
    pub fields: Vec<(&'static str, Value)>,
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

pub fn usage(message: impl Into<String>) -> anyhow::Error {
    CliError { code: EXIT_USAGE, message: message.into(), detail: None }.into()
}

pub fn conflict(message: impl Into<String>) -> anyhow::Error {
    CliError { code: EXIT_CONFLICT, message: message.into(), detail: None }.into()
}

/// A conflict the caller reports under `status` rather than its generic one, carrying `fields`
/// into the action so a caller has the same facts the message gives a reader.
pub fn conflict_as(status: &'static str, fields: Vec<(&'static str, Value)>, message: impl Into<String>) -> anyhow::Error {
    CliError { code: EXIT_CONFLICT, message: message.into(), detail: Some(Detail { status, fields }) }.into()
}

pub fn environment(message: impl Into<String>) -> anyhow::Error {
    CliError { code: EXIT_ENVIRONMENT, message: message.into(), detail: None }.into()
}

/// The same failure with `note` appended, for a caller saying what its run did not do. The exit
/// code and the detail the error already carries are kept, because naming the work left undone
/// is no reason to reclassify what went wrong.
pub fn annotated(err: anyhow::Error, note: &str) -> anyhow::Error {
    let cli = err.downcast_ref::<CliError>();
    let (code, detail) = (cli.map_or(EXIT_INTERNAL, |e| e.code), cli.and_then(|e| e.detail.clone()));
    CliError { code, message: format!("{err:#}; {note}"), detail }.into()
}

/// An environment error the caller reports under `status` rather than its generic one, carrying
/// `fields` into the action so a caller has the same facts the message gives a reader. The status
/// is also how a caller tells the one kind it can carry on past from the ones it cannot.
pub fn environment_as(status: &'static str, fields: Vec<(&'static str, Value)>, message: impl Into<String>) -> anyhow::Error {
    CliError { code: EXIT_ENVIRONMENT, message: message.into(), detail: Some(Detail { status, fields }) }.into()
}

pub fn is_conflict(err: &anyhow::Error) -> bool {
    err.downcast_ref::<CliError>().is_some_and(|e| e.code == EXIT_CONFLICT)
}

/// How the error asks to be reported, for the caller building the action.
pub fn detail_of(err: &anyhow::Error) -> Option<&Detail> {
    err.downcast_ref::<CliError>().and_then(|e| e.detail.as_ref())
}

pub fn exit_code(err: &anyhow::Error) -> i32 {
    err.downcast_ref::<CliError>().map_or(EXIT_INTERNAL, |e| e.code)
}
