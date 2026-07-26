// Log-file resolution and tailing for the remote console.
//
// The three exposed logs live in two dirs and follow two naming schemes,
// both mirroring what `tracing_appender::rolling::daily` writes:
//   Service -> <diagnostics>/service.log.<UTC-date>   (dated, rolls at midnight)
//   Widget  -> <logs>/widget.log.<UTC-date>           (dated, rolls at midnight)
//   Bench   -> <logs>/bench.log                        (plain TSV, not dated)
//
// Dirs are INJECTED (`LogDirs`) rather than derived from `current_exe()` so
// the core stays pure and Linux-testable; the service builds `LogDirs` from
// `DIAGNOSTICS_CACHE_DIR` + `logs_dir(exe_parent)` when it wires this up.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use chrono::NaiveDate;

/// The two directories the three logs live in. Injected so tests point at
/// tmp dirs and the pure core never calls `current_exe()`.
#[derive(Debug, Clone)]
pub struct LogDirs {
    pub diagnostics: PathBuf,
    pub logs: PathBuf,
}

/// The three logs the console exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFile {
    Service,
    Widget,
    Bench,
}

impl LogFile {
    /// All exposed logs, in tab / download order.
    pub const ALL: [LogFile; 3] = [LogFile::Service, LogFile::Widget, LogFile::Bench];

    /// Parse the `?file=` query value.
    pub fn from_param(s: &str) -> Option<LogFile> {
        match s {
            "service" => Some(LogFile::Service),
            "widget" => Some(LogFile::Widget),
            "bench" => Some(LogFile::Bench),
            _ => None,
        }
    }

    /// Stable identifier used in the API JSON and as the zip entry name.
    pub fn param(self) -> &'static str {
        match self {
            LogFile::Service => "service",
            LogFile::Widget => "widget",
            LogFile::Bench => "bench",
        }
    }

    /// Base filename (without the dated suffix).
    pub fn base(self) -> &'static str {
        match self {
            LogFile::Service => "service.log",
            LogFile::Widget => "widget.log",
            LogFile::Bench => "bench.log",
        }
    }

    /// Whether the file carries a `.<date>` suffix (rolls at UTC midnight).
    pub fn dated(self) -> bool {
        !matches!(self, LogFile::Bench)
    }

    fn dir(self, dirs: &LogDirs) -> &Path {
        match self {
            LogFile::Service => &dirs.diagnostics,
            LogFile::Widget | LogFile::Bench => &dirs.logs,
        }
    }
}

/// Path of the file to read *today*: dated logs resolve to `<base>.<date>`;
/// the plain log resolves to `<base>` itself.
pub fn today_path(dirs: &LogDirs, file: LogFile, today: NaiveDate) -> PathBuf {
    let dir = file.dir(dirs);
    if file.dated() {
        dir.join(format!("{}.{}", file.base(), today.format("%Y-%m-%d")))
    } else {
        dir.join(file.base())
    }
}

/// Every on-disk file for a log, oldest first — all dated files for a dated
/// log, or the single plain file. Used by the download-all zip.
pub fn history_files(dirs: &LogDirs, file: LogFile) -> Vec<PathBuf> {
    let dir = file.dir(dirs);
    if file.dated() {
        let prefix = format!("{}.", file.base());
        let mut v: Vec<PathBuf> = fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&prefix))
            })
            .collect();
        v.sort();
        v
    } else {
        let p = dir.join(file.base());
        if p.exists() {
            vec![p]
        } else {
            Vec::new()
        }
    }
}

/// Last `n` lines of `path` (or all lines when `n` is `None`). A missing
/// file is an empty snapshot — the log for a given day may not exist yet.
pub fn tail_snapshot(path: &Path, n: Option<usize>) -> Vec<String> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    match n {
        Some(n) if n < lines.len() => lines[lines.len() - n..].to_vec(),
        _ => lines,
    }
}

/// Incremental follower for the SSE stream. Keeps a byte offset into the
/// current file; each `poll` emits the newly-appended complete lines. For
/// dated logs it recomputes the filename every tick so it follows the
/// midnight rollover (new dated file -> offset resets to 0).
pub struct Tailer {
    file: LogFile,
    offset: u64,
    day: NaiveDate,
}

impl Tailer {
    /// Start following `file` as of `today`, from the current end of the
    /// file (SSE replays the snapshot separately, so the tail only carries
    /// lines written *after* the stream opened).
    pub fn new(dirs: &LogDirs, file: LogFile, today: NaiveDate) -> Self {
        let path = today_path(dirs, file, today);
        let offset = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        Tailer {
            file,
            offset,
            day: today,
        }
    }

    /// Read lines appended since the last poll. On a UTC day change (dated
    /// logs only) it switches to the new dated file with a fresh offset.
    /// Partial trailing lines are held back until their newline arrives.
    pub fn poll(&mut self, dirs: &LogDirs, today: NaiveDate) -> Vec<String> {
        if self.file.dated() && today != self.day {
            self.day = today;
            self.offset = 0;
        }
        let path = today_path(dirs, self.file, today);

        let Ok(mut f) = File::open(&path) else {
            return Vec::new();
        };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if len < self.offset {
            // File shrank (truncated / replaced) — restart from the top.
            self.offset = 0;
        }
        if f.seek(SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }
        let mut buf = Vec::new();
        if f.read_to_end(&mut buf).is_err() {
            return Vec::new();
        }

        match buf.iter().rposition(|&b| b == b'\n') {
            Some(nl) => {
                self.offset += (nl + 1) as u64;
                String::from_utf8_lossy(&buf[..=nl])
                    .lines()
                    .map(str::to_string)
                    .collect()
            }
            // No complete line yet — leave the partial for the next poll.
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn dirs() -> (tempfile::TempDir, LogDirs) {
        let tmp = tempfile::tempdir().unwrap();
        let diagnostics = tmp.path().join("diag");
        let logs = tmp.path().join("logs");
        fs::create_dir_all(&diagnostics).unwrap();
        fs::create_dir_all(&logs).unwrap();
        (tmp, LogDirs { diagnostics, logs })
    }

    fn append(path: &Path, s: &str) {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        f.write_all(s.as_bytes()).unwrap();
    }

    #[test]
    fn today_path_dated_vs_plain() {
        let (_tmp, d) = dirs();
        let t = day(2026, 7, 25);
        assert_eq!(
            today_path(&d, LogFile::Service, t),
            d.diagnostics.join("service.log.2026-07-25")
        );
        assert_eq!(
            today_path(&d, LogFile::Widget, t),
            d.logs.join("widget.log.2026-07-25")
        );
        assert_eq!(
            today_path(&d, LogFile::Bench, t),
            d.logs.join("bench.log")
        );
    }

    #[test]
    fn from_param_and_param_roundtrip() {
        for f in LogFile::ALL {
            assert_eq!(LogFile::from_param(f.param()), Some(f));
        }
        assert_eq!(LogFile::from_param("install"), None);
    }

    #[test]
    fn tail_snapshot_last_n_and_all() {
        let (_tmp, d) = dirs();
        let path = today_path(&d, LogFile::Bench, day(2026, 7, 25));
        append(&path, "l1\nl2\nl3\nl4\nl5\n");
        assert_eq!(tail_snapshot(&path, Some(2)), vec!["l4", "l5"]);
        assert_eq!(
            tail_snapshot(&path, None),
            vec!["l1", "l2", "l3", "l4", "l5"]
        );
        assert_eq!(tail_snapshot(&path, Some(99)), tail_snapshot(&path, None));
    }

    #[test]
    fn tail_snapshot_missing_file_is_empty() {
        let (_tmp, d) = dirs();
        let path = today_path(&d, LogFile::Service, day(2026, 7, 25));
        assert!(tail_snapshot(&path, None).is_empty());
    }

    #[test]
    fn tailer_emits_only_new_complete_lines() {
        let (_tmp, d) = dirs();
        let t = day(2026, 7, 25);
        let path = today_path(&d, LogFile::Service, t);
        append(&path, "existing\n");

        // Starts at end-of-file: the pre-existing line is not re-emitted.
        let mut tailer = Tailer::new(&d, LogFile::Service, t);
        assert!(tailer.poll(&d, t).is_empty());

        append(&path, "new1\nnew2\n");
        assert_eq!(tailer.poll(&d, t), vec!["new1", "new2"]);

        // A partial line (no newline) is withheld until completed.
        append(&path, "partial");
        assert!(tailer.poll(&d, t).is_empty());
        append(&path, "-done\n");
        assert_eq!(tailer.poll(&d, t), vec!["partial-done"]);

        // No change -> nothing.
        assert!(tailer.poll(&d, t).is_empty());
    }

    #[test]
    fn tailer_follows_midnight_rollover() {
        let (_tmp, d) = dirs();
        let d1 = day(2026, 7, 25);
        let d2 = day(2026, 7, 26);

        let p1 = today_path(&d, LogFile::Service, d1);
        append(&p1, "day1-a\n");
        let mut tailer = Tailer::new(&d, LogFile::Service, d1);
        append(&p1, "day1-b\n");
        assert_eq!(tailer.poll(&d, d1), vec!["day1-b"]);

        // Roll to the next UTC day: a new dated file, offset reset to 0, so
        // its first line is emitted even though byte offset would overshoot.
        let p2 = today_path(&d, LogFile::Service, d2);
        append(&p2, "day2-a\n");
        assert_eq!(tailer.poll(&d, d2), vec!["day2-a"]);
    }

    #[test]
    fn history_files_dated_sorted_and_plain_single() {
        let (_tmp, d) = dirs();
        append(
            &d.diagnostics.join("service.log.2026-07-26"),
            "newer\n",
        );
        append(
            &d.diagnostics.join("service.log.2026-07-25"),
            "older\n",
        );
        let dated = history_files(&d, LogFile::Service);
        assert_eq!(
            dated,
            vec![
                d.diagnostics.join("service.log.2026-07-25"),
                d.diagnostics.join("service.log.2026-07-26"),
            ]
        );

        append(&d.logs.join("bench.log"), "x\n");
        assert_eq!(
            history_files(&d, LogFile::Bench),
            vec![d.logs.join("bench.log")]
        );
    }

    #[test]
    fn history_files_absent_is_empty() {
        let (_tmp, d) = dirs();
        assert!(history_files(&d, LogFile::Widget).is_empty());
        assert!(history_files(&d, LogFile::Bench).is_empty());
    }
}
