//! The session log store: full text of shortened command and MCP output, and
//! background jobs' output, within one 64 MiB budget per session.
//!
//! Logs are private files outside every workspace and sandbox. Only a direct
//! `read_file` from the owning session opens them, and the store removes them
//! when the session ends. See
//! `docs/develop/EnvironmentInterface.md#session-logs`.

use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::PathBuf,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
};

use super::LOGS_PREFIX;

/// The per-session budget for log content.
pub const LOG_BUDGET: u64 = 64 * 1024 * 1024;

#[derive(Debug)]
pub struct SessionLogs {
    dir: PathBuf,
    budget: u64,
    used: Mutex<u64>,
    next: AtomicU64,
}

impl SessionLogs {
    /// A store in `dir`, created on first use with mode 0700.
    pub fn new(dir: PathBuf) -> Self {
        Self::with_budget(dir, LOG_BUDGET)
    }

    pub fn with_budget(dir: PathBuf, budget: u64) -> Self {
        Self {
            dir,
            budget,
            used: Mutex::new(0),
            next: AtomicU64::new(1),
        }
    }

    /// Reserves space before writing. False when the budget is used up.
    pub fn reserve(&self, bytes: u64) -> bool {
        let mut used = self.used.lock().unwrap_or_else(PoisonError::into_inner);
        if *used + bytes > self.budget {
            return false;
        }
        *used += bytes;
        true
    }

    pub fn release(&self, bytes: u64) {
        let mut used = self.used.lock().unwrap_or_else(PoisonError::into_inner);
        *used = used.saturating_sub(bytes);
    }

    /// A new private file in the store and its model-facing path.
    pub fn create(&self, prefix: &str) -> std::io::Result<(File, String)> {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)?;
        let name = format!("{prefix}-{}.log", self.next.fetch_add(1, Ordering::Relaxed));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.dir.join(&name))?;
        Ok((file, format!("{LOGS_PREFIX}/{name}")))
    }

    /// Opens an issued log for reading. `name` is the part after the prefix.
    pub(crate) fn open(&self, name: &str) -> std::io::Result<File> {
        let name = name.trim_start_matches('/');
        if name.is_empty() || name.contains('/') || name.starts_with('.') {
            return Err(std::io::ErrorKind::NotFound.into());
        }
        OpenOptions::new()
            .read(true)
            .custom_flags(libc_nofollow())
            .open(self.dir.join(name))
    }

    /// Removes every log. Called when the session ends.
    pub fn remove_all(&self) {
        let _ = fs::remove_dir_all(&self.dir);
        *self.used.lock().unwrap_or_else(PoisonError::into_inner) = 0;
    }
}

impl Drop for SessionLogs {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn libc_nofollow() -> i32 {
    rustix::fs::OFlags::NOFOLLOW.bits() as i32
}

/// The complete text of a command or MCP result, held until the Agent Loop
/// has sized its preview.
#[derive(Debug)]
pub struct LogCapture {
    logs: Arc<SessionLogs>,
    reserved: u64,
    /// Why the full text cannot be kept, when the budget ran out while
    /// capturing.
    pub unavailable: Option<String>,
    prefix: &'static str,
}

impl LogCapture {
    /// Reserves space for `len` bytes of captured text.
    pub fn new(logs: Arc<SessionLogs>, len: u64, prefix: &'static str) -> Self {
        let (reserved, unavailable) = if logs.reserve(len) {
            (len, None)
        } else {
            (
                0,
                Some("the session's 64 MiB log budget is used up".to_owned()),
            )
        };
        Self {
            logs,
            reserved,
            unavailable,
            prefix,
        }
    }

    /// Saves the text and returns its `log_path`, keeping its space reserved.
    pub fn save(mut self, text: &str) -> Result<String, String> {
        if let Some(reason) = self.unavailable.take() {
            return Err(reason);
        }
        let result = self
            .logs
            .create(self.prefix)
            .and_then(|(mut file, path)| file.write_all(text.as_bytes()).map(|()| path));
        match result {
            Ok(path) => {
                self.reserved = 0;
                Ok(path)
            }
            Err(error) => Err(format!("saving the log failed: {error}")),
        }
    }
}

impl Drop for LogCapture {
    fn drop(&mut self) {
        self.logs.release(self.reserved);
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    #[test]
    fn captures_save_within_the_budget_and_open_by_name() {
        let root = tempfile::tempdir().unwrap();
        let logs = Arc::new(SessionLogs::with_budget(root.path().join("logs"), 10));
        let path = LogCapture::new(logs.clone(), 6, "cmd")
            .save("abcdef")
            .unwrap();
        assert!(path.starts_with("/session-logs/cmd-"));
        let mut text = String::new();
        logs.open(path.trim_start_matches(LOGS_PREFIX))
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "abcdef");

        // A released capture frees its space; a saved one keeps it.
        drop(LogCapture::new(logs.clone(), 4, "cmd"));
        let over = LogCapture::new(logs.clone(), 5, "cmd");
        assert!(over.unavailable.is_some());
        assert!(over.save("12345").is_err());
        assert!(logs.open("../x").is_err());

        logs.remove_all();
        assert!(!root.path().join("logs").exists());
    }
}
