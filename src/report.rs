//! Output in either of two shapes: one human-readable line per action followed by a
//! `key=value` summary, or a single JSON document for `--json`. Actions carry database names
//! and file paths only; connection strings never pass through here.

use serde_json::{json, Map, Value};
use std::io::Write;

pub struct Reporter {
    json: bool,
    quiet: bool,
    verbose: bool,
    actions: Vec<Value>,
}

impl Reporter {
    pub fn new(json: bool, quiet: bool, verbose: bool) -> Self {
        Self { json, quiet, verbose, actions: Vec::new() }
    }

    pub fn action(&mut self, action: Value, line: impl AsRef<str>) {
        self.actions.push(action);
        self.print(line.as_ref());
    }

    pub fn info(&self, line: impl AsRef<str>) {
        self.print(line.as_ref());
    }

    /// Whether `verbose` lines are printed, so a caller can skip the work of building one.
    pub fn is_verbose(&self) -> bool {
        self.verbose
    }

    pub fn verbose(&self, line: impl AsRef<str>) {
        if self.verbose {
            self.print(line.as_ref());
        }
    }

    pub fn warn(&self, line: impl AsRef<str>) {
        if !self.json {
            eprintln!("warning: {}", line.as_ref());
        }
    }

    /// Emits the JSON document (with the collected actions attached) or the summary line.
    pub fn finish(&self, mut document: Map<String, Value>, summary: &Counts) {
        if self.json {
            document.insert("summary".into(), summary.to_json());
            document.insert("actions".into(), Value::Array(self.actions.clone()));
            println!("{}", serde_json::to_string_pretty(&Value::Object(document)).expect("json"));
        } else if !self.quiet {
            println!("{}", summary.line());
        }
        let _ = std::io::stdout().flush();
    }

    fn print(&self, line: &str) {
        if !self.json && !self.quiet {
            println!("{line}");
        }
    }
}

/// Ordered counters for a command's summary.
#[derive(Default)]
pub struct Counts {
    entries: Vec<(&'static str, usize)>,
}

impl Counts {
    pub fn new(keys: &[&'static str]) -> Self {
        Self { entries: keys.iter().map(|k| (*k, 0)).collect() }
    }

    pub fn inc(&mut self, key: &'static str) {
        self.add(key, 1);
    }

    pub fn add(&mut self, key: &'static str, n: usize) {
        match self.entries.iter_mut().find(|(k, _)| *k == key) {
            Some(entry) => entry.1 += n,
            None => self.entries.push((key, n)),
        }
    }

    pub fn get(&self, key: &str) -> usize {
        self.entries.iter().find(|(k, _)| *k == key).map_or(0, |e| e.1)
    }

    /// Renames a counter, used to report `created` as `create_planned` in a dry run.
    pub fn rename(&mut self, from: &str, to: &'static str) {
        if let Some(entry) = self.entries.iter_mut().find(|(k, _)| *k == from) {
            entry.0 = to;
        }
    }

    pub fn line(&self) -> String {
        self.entries.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(" ")
    }

    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        for (k, v) in &self.entries {
            map.insert((*k).into(), json!(v));
        }
        Value::Object(map)
    }
}
