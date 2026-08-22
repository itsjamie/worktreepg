//! Edits to a dotenv file that leave every byte outside the targeted assignment lines intact:
//! quoting style, `export` prefixes, spacing around `=`, trailing comments, and line endings
//! all survive a rewrite.

use regex::Regex;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub struct EnvFile {
    path: PathBuf,
    content: String,
    dirty: bool,
}

impl EnvFile {
    /// Fails with `NotFound` when the file is absent; an env file is never created here.
    pub fn open(path: &Path) -> io::Result<Self> {
        Ok(Self { path: path.to_path_buf(), content: fs::read_to_string(path)?, dirty: false })
    }

    pub fn from_content(path: &Path, content: String) -> Self {
        Self { path: path.to_path_buf(), content, dirty: false }
    }

    /// Value of the last assignment of `name`, matching dotenv's last-wins rule.
    pub fn get(&self, name: &str) -> Option<String> {
        find_assignments(&self.content, name).pop().map(|a| a.value)
    }

    /// Rewrites every assignment of `name`, or appends one when there is none.
    pub fn set(&mut self, name: &str, value: &str) {
        let found = find_assignments(&self.content, name);
        if found.is_empty() {
            let sep = if self.content.is_empty() || self.content.ends_with('\n') { "" } else { "\n" };
            self.content.push_str(&format!("{sep}{name}={value}\n"));
        } else {
            let mut lines: Vec<String> = self.content.split('\n').map(str::to_string).collect();
            for a in &found {
                lines[a.index] = format!("{}{}{}{}{}", a.prefix, a.quote, value, a.quote, a.suffix);
            }
            self.content = lines.join("\n");
        }
        self.dirty = true;
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    /// Writes the file only if `set` was called.
    pub fn save(&mut self) -> io::Result<()> {
        if self.dirty {
            fs::write(&self.path, &self.content)?;
            self.dirty = false;
        }
        Ok(())
    }
}

struct Assignment {
    index: usize,
    prefix: String,
    quote: &'static str,
    value: String,
    suffix: String,
}

fn assignment_re(name: &str) -> Regex {
    Regex::new(&format!(r"^(\s*(?:export\s+)?{}\s*=\s*)(.*)$", regex::escape(name))).unwrap()
}

/// Splits the right-hand side of an assignment into quote, value, and whatever follows.
fn split_value(raw: &str) -> (&'static str, String, String) {
    for (quote, ch) in [("\"", '"'), ("'", '\''), ("`", '`')] {
        if raw.starts_with(ch) {
            if let Some(end) = raw[1..].find(ch) {
                return (quote, raw[1..=end].to_string(), raw[end + 2..].to_string());
            }
        }
    }
    // Unquoted: dotenv ends the value at the first `#` preceded by whitespace.
    let mut value_end = raw.trim_end().len();
    if let Some(hash) = raw.find('#') {
        if raw[..hash].ends_with(char::is_whitespace) {
            value_end = raw[..hash].trim_end().len();
        }
    }
    ("", raw[..value_end].to_string(), raw[value_end..].to_string())
}

fn find_assignments(content: &str, name: &str) -> Vec<Assignment> {
    let re = assignment_re(name);
    let mut found = Vec::new();
    for (index, line) in content.split('\n').enumerate() {
        let (body, cr) = match line.strip_suffix('\r') {
            Some(b) => (b, "\r"),
            None => (line, ""),
        };
        let Some(caps) = re.captures(body) else { continue };
        let (quote, value, suffix) = split_value(caps.get(2).map_or("", |m| m.as_str()));
        found.push(Assignment {
            index,
            prefix: caps.get(1).map_or("", |m| m.as_str()).to_string(),
            quote,
            value,
            suffix: format!("{suffix}{cr}"),
        });
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "# database\nDATABASE_URL=\"postgres://u:p@localhost:5432/app?sslmode=disable\"\nexport DIRECT_URL = 'postgres://u:p@localhost:5432/app'\nPLAIN_URL=postgres://u:p@localhost:5432/app # primary\nOTHER=1\n";

    fn env(content: &str) -> EnvFile {
        EnvFile::from_content(Path::new("/dev/null"), content.to_string())
    }

    #[test]
    fn reads_quoted_exported_and_commented_values() {
        let f = env(FILE);
        assert_eq!(f.get("DATABASE_URL").as_deref(), Some("postgres://u:p@localhost:5432/app?sslmode=disable"));
        assert_eq!(f.get("DIRECT_URL").as_deref(), Some("postgres://u:p@localhost:5432/app"));
        assert_eq!(f.get("PLAIN_URL").as_deref(), Some("postgres://u:p@localhost:5432/app"));
        assert_eq!(f.get("MISSING"), None);
    }

    #[test]
    fn last_assignment_wins() {
        assert_eq!(env("A=1\nA=2\n").get("A").as_deref(), Some("2"));
    }

    #[test]
    fn set_preserves_everything_else() {
        let mut f = env(FILE);
        f.set("PLAIN_URL", "postgres://u:p@localhost:5432/app_fork");
        assert_eq!(
            f.content(),
            FILE.replace(
                "PLAIN_URL=postgres://u:p@localhost:5432/app # primary",
                "PLAIN_URL=postgres://u:p@localhost:5432/app_fork # primary"
            )
        );
        f.set("DIRECT_URL", "x");
        assert!(f.content().contains("export DIRECT_URL = 'x'\n"));
    }

    #[test]
    fn set_ignores_prefix_matches_and_appends_when_missing() {
        let mut f = env("DATABASE_URL_TEST=a\nDATABASE_URL=b\n");
        f.set("DATABASE_URL", "c");
        assert_eq!(f.content(), "DATABASE_URL_TEST=a\nDATABASE_URL=c\n");
        f.set("NEW", "1");
        assert_eq!(f.content(), "DATABASE_URL_TEST=a\nDATABASE_URL=c\nNEW=1\n");
        let mut g = env("A=1");
        g.set("B", "2");
        assert_eq!(g.content(), "A=1\nB=2\n");
    }

    #[test]
    fn set_keeps_crlf() {
        let mut f = env("A=1\r\nB=2\r\n");
        f.set("A", "9");
        assert_eq!(f.content(), "A=9\r\nB=2\r\n");
    }
}
