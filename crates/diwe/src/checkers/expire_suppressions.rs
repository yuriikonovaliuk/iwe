//! Rust port of the live docs store's suppression-expiry rule.
//!
//! Behavioral source (sole source, per the porting task): the live store's
//! `scripts/checkers/expire-suppressions.sh`, which `.githooks/pre-commit`
//! runs before `iwe schema validate` — that store sets `core.hooksPath =
//! .githooks`, so `.git/hooks/pre-commit` there is unused. The shell
//! wrapper just execs `scripts/checkers/expire-suppressions.py`, which is
//! the actual rule this module mirrors:
//!
//! - It has **no marker file and no mtime of any kind**. It rereads
//!   `.iwe/config.toml` as plain text (not TOML — iwe's parser treats
//!   these as comments) and matches every line against
//!   `# suppress: <name> until=<YYYY-MM-DD> reason="<text>"`.
//! - Each matched line's `until` is compared against *today's date*
//!   (`datetime.date.today()`); `until < today` is expired.
//! - An `until` that isn't a valid ISO date makes the script refuse to
//!   enforce and exit non-zero (blocking), rather than silently ignoring
//!   the line — mirrored here by treating it as expired too.
//!
//! Because there is no marker file, `SuppressionExpiryViolation::marker_path`
//! — a field the porting task's shared surface fixed before this mechanism
//! was inspected — is set to the path of `.iwe/config.toml` itself, the
//! only file this mechanism ever reads or could be said to "mark". There is
//! no mtime-preservation risk to flag for 4b/6: the real script never reads
//! or writes an mtime; the enforcement is the calendar date written in the
//! `until=` text, not any filesystem timestamp.

use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use regex::Regex;

/// One `# suppress:` line in `.iwe/config.toml` whose `until` has passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuppressionExpiryViolation {
    /// The suppressed checker's name, e.g. `no-forbidden` in
    /// `# suppress: no-forbidden until=2026-01-01 reason="..."`.
    pub key: String,
    /// Where the expired line lives — `.iwe/config.toml` itself; this
    /// mechanism has no separate marker file (see module docs).
    pub marker_path: PathBuf,
    /// The suppression's `until` date (its expiry). `NaiveDate::MIN` for a
    /// line whose `until` failed to parse as an ISO date — the real script
    /// refuses to enforce such a line and blocks the commit too.
    pub expiry: NaiveDate,
}

/// `# suppress: <name> until=<YYYY-MM-DD> reason="<free text>"`, tolerating
/// extra whitespace — the exact grammar `expire-suppressions.py` matches.
fn suppress_line_pattern() -> Regex {
    Regex::new(
        r#"^\s*#\s*suppress:\s*(?P<name>[A-Za-z0-9_-]+)\s+until\s*=\s*(?P<until>\d{4}-\d{2}-\d{2})\s+reason\s*=\s*"[^"]*"\s*$"#,
    )
    .expect("static suppress-line pattern is valid")
}

/// The unconditional always-checker for suppression expiry: no config
/// toggle, no suppression-window opt-out, no affected-set gate — the
/// caller (`validating_transaction.rs`) runs this on every `validate =
/// "full"` commit regardless of what the transaction touched, since a
/// calendar expiry can make a suppression stale independent of any write.
pub struct ExpireSuppressionsChecker;

impl ExpireSuppressionsChecker {
    /// Expired suppressions in `<root>/.iwe/config.toml`, as of today.
    pub fn check(&self, root: &Path) -> Vec<SuppressionExpiryViolation> {
        self.check_on(root, chrono::Local::now().date_naive())
    }

    /// [`Self::check`] with an explicit "today", for deterministic tests.
    pub fn check_on(&self, root: &Path, today: NaiveDate) -> Vec<SuppressionExpiryViolation> {
        let config_path = root.join(crate::config::IWE_MARKER).join("config.toml");
        let Ok(contents) = std::fs::read_to_string(&config_path) else {
            // No config, nothing to enforce — matches the shell script's
            // "config not found" case, which exits 0.
            return Vec::new();
        };
        Self::scan(&contents, &config_path, today)
    }

    fn scan(contents: &str, config_path: &Path, today: NaiveDate) -> Vec<SuppressionExpiryViolation> {
        let pattern = suppress_line_pattern();
        let mut expired = Vec::new();
        for line in contents.lines() {
            let Some(captures) = pattern.captures(line) else {
                continue;
            };
            let name = captures["name"].to_string();
            let until_raw = &captures["until"];
            let until = NaiveDate::parse_from_str(until_raw, "%Y-%m-%d").unwrap_or(NaiveDate::MIN);
            if until < today {
                expired.push(SuppressionExpiryViolation {
                    key: name,
                    marker_path: config_path.to_path_buf(),
                    expiry: until,
                });
            }
        }
        expired
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 9, 5).unwrap()
    }

    fn write_config(root: &Path, body: &str) {
        fs::create_dir_all(root.join(".iwe")).unwrap();
        fs::write(root.join(".iwe/config.toml"), body).unwrap();
    }

    #[test]
    fn no_suppress_lines_is_clean() {
        let temp = TempDir::new().unwrap();
        write_config(temp.path(), "version = 3\n");
        assert!(ExpireSuppressionsChecker
            .check_on(temp.path(), today())
            .is_empty());
    }

    #[test]
    fn future_until_is_clean() {
        let temp = TempDir::new().unwrap();
        write_config(
            temp.path(),
            "# suppress: mint-stub until=2026-12-31 reason=\"mid-effort\"\n",
        );
        assert!(ExpireSuppressionsChecker
            .check_on(temp.path(), today())
            .is_empty());
    }

    #[test]
    fn past_until_is_expired() {
        let temp = TempDir::new().unwrap();
        write_config(
            temp.path(),
            "# suppress: mint-stub until=2026-01-01 reason=\"mid-effort\"\n",
        );
        let expired = ExpireSuppressionsChecker.check_on(temp.path(), today());
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].key, "mint-stub");
        assert_eq!(expired[0].expiry, NaiveDate::from_ymd_opt(2026, 1, 1).unwrap());
        assert_eq!(expired[0].marker_path, temp.path().join(".iwe/config.toml"));
    }

    #[test]
    fn multiple_expired_lines_all_reported() {
        let temp = TempDir::new().unwrap();
        write_config(
            temp.path(),
            "# suppress: a until=2026-01-01 reason=\"one\"\n\
             # suppress: b until=2026-02-02 reason=\"two\"\n\
             # suppress: c until=2099-01-01 reason=\"not expired\"\n",
        );
        let expired = ExpireSuppressionsChecker.check_on(temp.path(), today());
        let keys: Vec<&str> = expired.iter().map(|v| v.key.as_str()).collect();
        assert_eq!(keys, vec!["a", "b"]);
    }

    #[test]
    fn missing_config_is_clean() {
        let temp = TempDir::new().unwrap();
        assert!(ExpireSuppressionsChecker
            .check_on(temp.path(), today())
            .is_empty());
    }
}
