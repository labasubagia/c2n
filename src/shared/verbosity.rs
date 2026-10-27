//! User-facing verbosity, stage counters, progress output, and fail logs.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use indicatif::ProgressBar;
use serde::Serialize;

use crate::shared::time::now_utc_iso;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verbosity {
    Quiet,
    Simple,
    Verbose,
}

#[derive(Debug, Clone)]
pub struct Reporter {
    inner: Arc<ReporterInner>,
}

#[derive(Debug)]
struct ReporterInner {
    verbosity: Verbosity,
    label: String,
    counts: Mutex<Vec<(String, usize)>>,
    fail_log: Option<PathBuf>,
    progress: Mutex<Option<ProgressBar>>,
}

impl Reporter {
    pub fn new(verbosity: Verbosity, fail_log: Option<PathBuf>) -> Self {
        Self {
            inner: Arc::new(ReporterInner {
                verbosity,
                label: "Processed".to_owned(),
                counts: Mutex::new(Vec::new()),
                fail_log,
                progress: Mutex::new(None),
            }),
        }
    }

    pub fn verbosity(&self) -> Verbosity {
        self.inner.verbosity
    }

    pub fn is_verbose(&self) -> bool {
        self.inner.verbosity == Verbosity::Verbose
    }

    pub fn is_quiet(&self) -> bool {
        self.inner.verbosity == Verbosity::Quiet
    }

    pub fn set_progress(&self, progress: ProgressBar) {
        if let Ok(mut current) = self.inner.progress.lock() {
            *current = Some(progress);
        }
    }

    pub fn clear_progress(&self) {
        if let Ok(mut current) = self.inner.progress.lock()
            && let Some(progress) = current.take()
        {
            progress.finish_and_clear();
        }
    }

    /// Print a line, keeping the progress bar intact.
    pub fn println(&self, message: impl AsRef<str>) {
        let message = message.as_ref();
        if let Ok(progress) = self.inner.progress.lock()
            && let Some(progress) = progress.as_ref()
        {
            progress.println(message);
            return;
        }
        println!("{message}");
    }

    /// Normal output, suppressed by `--quiet`.
    pub fn info(&self, message: impl AsRef<str>) {
        if !self.is_quiet() {
            self.println(message);
        }
    }

    /// Per-file output, only shown with `--verbose`.
    pub fn verbose(&self, message: impl AsRef<str>) {
        if self.is_verbose() {
            self.println(message);
        }
    }

    pub fn warn(&self, message: impl AsRef<str>) {
        eprintln!("Warning: {}", message.as_ref());
    }

    pub fn error(&self, message: impl AsRef<str>) {
        eprintln!("Error: {}", message.as_ref());
    }

    pub fn add_stage(&self, category: &str, count: usize) {
        if count == 0 {
            return;
        }
        if let Ok(mut counts) = self.inner.counts.lock() {
            if let Some((_, value)) = counts.iter_mut().find(|(name, _)| name == category) {
                *value += count;
            } else {
                counts.push((category.to_owned(), count));
            }
        }
    }

    pub fn summary_line(&self) -> String {
        let counts = self
            .inner
            .counts
            .lock()
            .map(|counts| counts.clone())
            .unwrap_or_default();
        let parts: Vec<String> = counts
            .into_iter()
            .filter(|(_, count)| *count > 0)
            .map(|(category, count)| format!("{count} {category}"))
            .collect();
        if parts.is_empty() {
            format!("{}: nothing to do", self.inner.label)
        } else {
            format!("{}: {}", self.inner.label, parts.join(", "))
        }
    }

    pub fn write_fail_log(&self, entry: &FailLogEntry) {
        let Some(path) = self.inner.fail_log.as_ref() else {
            return;
        };
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            self.warn(format!("could not create fail log directory: {error}"));
            return;
        }

        let line = match serde_json::to_string(entry) {
            Ok(line) => line,
            Err(error) => {
                self.warn(format!("could not serialize fail log entry: {error}"));
                return;
            }
        };

        match OpenOptions::new().create(true).append(true).open(path) {
            Ok(mut file) => {
                if let Err(error) = writeln!(file, "{line}") {
                    self.warn(format!("could not write fail log: {error}"));
                }
            }
            Err(error) => self.warn(format!("could not open fail log: {error}")),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct FailLogEntry {
    pub id: String,
    pub conversation_id: String,
    pub url: String,
    pub prompt: String,
    pub stage: String,
    pub file: String,
    pub error: String,
    pub timestamp: String,
}

impl FailLogEntry {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: impl Into<String>,
        conversation_id: impl Into<String>,
        url: impl Into<String>,
        prompt: impl Into<String>,
        stage: impl Into<String>,
        file: impl Into<String>,
        error: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            conversation_id: conversation_id.into(),
            url: url.into(),
            prompt: prompt.into(),
            stage: stage.into(),
            file: file.into(),
            error: error.into(),
            timestamp: now_utc_iso(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_summary_is_stable() {
        let reporter = Reporter::new(Verbosity::Simple, None);
        reporter.add_stage("success", 2);
        reporter.add_stage("failed", 1);
        reporter.add_stage("success", 1);
        assert_eq!(reporter.summary_line(), "Processed: 3 success, 1 failed");
    }

    #[test]
    fn empty_summary_says_nothing_to_do() {
        let reporter = Reporter::new(Verbosity::Simple, None);
        assert_eq!(reporter.summary_line(), "Processed: nothing to do");
    }
}
