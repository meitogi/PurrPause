// Log-file resolution and tailing for the remote console.
//
// The four exposed logs live in two dirs; all are dated daily, mirroring what
// `tracing_appender::rolling::daily` writes (`<base>.<UTC-date>`, rolled at
// midnight):
//   Service  -> <diagnostics>/service.log.<date>
//   Widget   -> <logs>/widget.log.<date>
//   Install  -> <logs>/install.log.<date>
//   Watchdog -> <diagnostics>/watchdog.log.<date>
//
// Service/Widget/Watchdog are shown as a 2-day window (today + yesterday) with
// the live tail on today; Install resolves to its latest available dated file
// (installs are past events, so "today" is usually empty).
//
// Dirs are INJECTED (`LogDirs`) rather than derived from `current_exe()` so
// the core stays pure and Linux-testable; the service builds `LogDirs` from
// `DIAGNOSTICS_CACHE_DIR` + `logs_dir(exe_parent)` when it wires this up.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use chrono::NaiveDate;

/// The two directories the logs live in. Injected so tests point at tmp dirs
/// and the pure core never calls `current_exe()`.
#[derive(Debug, Clone)]
pub struct LogDirs {
    pub diagnostics: PathBuf,
    pub logs: PathBuf,
}

/// The four logs the console exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFile {
    Service,
    Widget,
    Install,
    Watchdog,
}

/// How a log's displayed history and tail target are resolved by date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateMode {
    /// Today + yesterday shown; the live tail follows today (rolls at midnight).
    Window,
    /// The newest available dated file; no rollover (installs are past events).
    Latest,
}

impl LogFile {
    /// All exposed logs, in tab / download order.
    pub const ALL: [LogFile; 4] = [
        LogFile::Service,
        LogFile::Widget,
        LogFile::Install,
        LogFile::Watchdog,
    ];

    /// Parse the `?file=` query value.
    pub fn from_param(s: &str) -> Option<LogFile> {
        match s {
            "service" => Some(LogFile::Service),
            "widget" => Some(LogFile::Widget),
            "install" => Some(LogFile::Install),
            "watchdog" => Some(LogFile::Watchdog),
            _ => None,
        }
    }

    /// Stable identifier used in the API JSON and as the zip entry name.
    pub fn param(self) -> &'static str {
        match self {
            LogFile::Service => "service",
            LogFile::Widget => "widget",
            LogFile::Install => "install",
            LogFile::Watchdog => "watchdog",
        }
    }

    /// Base filename (without the dated suffix).
    pub fn base(self) -> &'static str {
        match self {
            LogFile::Service => "service.log",
            LogFile::Widget => "widget.log",
            LogFile::Install => "install.log",
            LogFile::Watchdog => "watchdog.log",
        }
    }

    /// How this log's history and tail resolve by date.
    pub fn date_mode(self) -> DateMode {
        match self {
            LogFile::Install => DateMode::Latest,
            _ => DateMode::Window,
        }
    }

    fn dir(self, dirs: &LogDirs) -> &Path {
        match self {
            LogFile::Service | LogFile::Watchdog => &dirs.diagnostics,
            LogFile::Widget | LogFile::Install => &dirs.logs,
        }
    }
}

/// Path of the dated file for `day`: `<base>.<date>`.
pub fn today_path(dirs: &LogDirs, file: LogFile, today: NaiveDate) -> PathBuf {
    file.dir(dirs)
        .join(format!("{}.{}", file.base(), today.format("%Y-%m-%d")))
}

/// Every on-disk dated file for a log, oldest first. Used by the download-all
/// zip and by `latest_path`.
pub fn history_files(dirs: &LogDirs, file: LogFile) -> Vec<PathBuf> {
    let dir = file.dir(dirs);
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
}

/// Newest on-disk dated file for a log; falls back to today's path when none
/// exist (yielding an empty snapshot). Used by `Latest`-mode logs (Install).
pub fn latest_path(dirs: &LogDirs, file: LogFile, today: NaiveDate) -> PathBuf {
    history_files(dirs, file)
        .pop()
        .unwrap_or_else(|| today_path(dirs, file, today))
}

/// The files backing a log's displayed history, in chronological order.
/// `Window` logs show yesterday + today (missing files yield empty snapshots,
/// so both are always listed); `Latest` logs show their newest dated file.
pub fn resolve_paths(dirs: &LogDirs, file: LogFile, today: NaiveDate) -> Vec<PathBuf> {
    match file.date_mode() {
        DateMode::Window => {
            let yesterday = today.pred_opt().unwrap_or(today);
            vec![
                today_path(dirs, file, yesterday),
                today_path(dirs, file, today),
            ]
        }
        DateMode::Latest => vec![latest_path(dirs, file, today)],
    }
}

/// Displayed history for a log: the concatenation of its `resolve_paths` files,
/// capped to the last `n` lines of the combined window when `n` is set.
pub fn snapshot(dirs: &LogDirs, file: LogFile, today: NaiveDate, n: Option<usize>) -> Vec<String> {
    let mut lines = Vec::new();
    for path in resolve_paths(dirs, file, today) {
        lines.extend(tail_snapshot(&path, None));
    }
    match n {
        Some(n) if n < lines.len() => lines.split_off(lines.len() - n),
        _ => lines,
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
/// pinned file; each `poll` emits the newly-appended complete lines. `Window`
/// logs follow the midnight rollover (new dated file -> offset resets to 0);
/// `Latest` logs stay pinned to the file resolved at construction.
pub struct Tailer {
    file: LogFile,
    mode: DateMode,
    offset: u64,
    day: NaiveDate,
    path: PathBuf,
}

impl Tailer {
    /// Start following `file` as of `today`, from the current end of the
    /// resolved file (SSE carries only lines written *after* the stream
    /// opened; the snapshot is served separately by `/api/logs`).
    pub fn new(dirs: &LogDirs, file: LogFile, today: NaiveDate) -> Self {
        let mode = file.date_mode();
        let path = match mode {
            DateMode::Window => today_path(dirs, file, today),
            DateMode::Latest => latest_path(dirs, file, today),
        };
        let offset = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        Tailer {
            file,
            mode,
            offset,
            day: today,
            path,
        }
    }

    /// Read lines appended since the last poll. On a UTC day change (`Window`
    /// logs only) it switches to the new dated file with a fresh offset.
    /// Partial trailing lines are held back until their newline arrives.
    pub fn poll(&mut self, dirs: &LogDirs, today: NaiveDate) -> Vec<String> {
        if self.mode == DateMode::Window && today != self.day {
            self.day = today;
            self.offset = 0;
            self.path = today_path(dirs, self.file, today);
        }

        let Ok(mut f) = File::open(&self.path) else {
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
    fn today_path_maps_to_correct_dir() {
        let (_tmp, d) = dirs();
        let t = day(2026, 7, 25);
        // Service + Watchdog land in DiagnosticsCache.
        assert_eq!(
            today_path(&d, LogFile::Service, t),
            d.diagnostics.join("service.log.2026-07-25")
        );
        assert_eq!(
            today_path(&d, LogFile::Watchdog, t),
            d.diagnostics.join("watchdog.log.2026-07-25")
        );
        // Widget + Install land in Data\Logs.
        assert_eq!(
            today_path(&d, LogFile::Widget, t),
            d.logs.join("widget.log.2026-07-25")
        );
        assert_eq!(
            today_path(&d, LogFile::Install, t),
            d.logs.join("install.log.2026-07-25")
        );
    }

    #[test]
    fn from_param_and_param_roundtrip() {
        for f in LogFile::ALL {
            assert_eq!(LogFile::from_param(f.param()), Some(f));
        }
        // The dropped Bench alias no longer resolves.
        assert_eq!(LogFile::from_param("bench"), None);
        assert_eq!(LogFile::from_param(""), None);
    }

    #[test]
    fn date_mode_install_is_latest_others_window() {
        assert_eq!(LogFile::Install.date_mode(), DateMode::Latest);
        for f in [LogFile::Service, LogFile::Widget, LogFile::Watchdog] {
            assert_eq!(f.date_mode(), DateMode::Window);
        }
    }

    #[test]
    fn tail_snapshot_last_n_and_all() {
        let (_tmp, d) = dirs();
        let path = today_path(&d, LogFile::Widget, day(2026, 7, 25));
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
    fn latest_path_picks_newest_or_falls_back_to_today() {
        let (_tmp, d) = dirs();
        let today = day(2026, 7, 26);
        // No files yet -> today's path (an empty snapshot target).
        assert_eq!(
            latest_path(&d, LogFile::Install, today),
            today_path(&d, LogFile::Install, today)
        );

        append(&d.logs.join("install.log.2026-07-20"), "older\n");
        append(&d.logs.join("install.log.2026-07-24"), "newer\n");
        assert_eq!(
            latest_path(&d, LogFile::Install, today),
            d.logs.join("install.log.2026-07-24")
        );
    }

    #[test]
    fn snapshot_window_concatenates_yesterday_then_today() {
        let (_tmp, d) = dirs();
        let today = day(2026, 7, 26);
        let yesterday = day(2026, 7, 25);
        append(&today_path(&d, LogFile::Service, yesterday), "y1\ny2\n");
        append(&today_path(&d, LogFile::Service, today), "t1\nt2\n");

        // Whole window, yesterday first.
        assert_eq!(
            snapshot(&d, LogFile::Service, today, None),
            vec!["y1", "y2", "t1", "t2"]
        );
        // Tail limit spans the combined window.
        assert_eq!(
            snapshot(&d, LogFile::Service, today, Some(3)),
            vec!["y2", "t1", "t2"]
        );
    }

    #[test]
    fn snapshot_latest_reads_only_newest_file() {
        let (_tmp, d) = dirs();
        let today = day(2026, 7, 26);
        append(&d.logs.join("install.log.2026-07-20"), "old\n");
        append(&d.logs.join("install.log.2026-07-24"), "recent\n");
        assert_eq!(
            snapshot(&d, LogFile::Install, today, None),
            vec!["recent"]
        );
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
    fn tailer_window_follows_midnight_rollover() {
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
    fn tailer_latest_pins_and_ignores_rollover() {
        let (_tmp, d) = dirs();
        let today = day(2026, 7, 26);
        let path = d.logs.join("install.log.2026-07-24");
        append(&path, "installed\n");

        // Pinned to the latest file at EOF; the pre-existing line isn't re-emitted.
        let mut tailer = Tailer::new(&d, LogFile::Install, today);
        assert!(tailer.poll(&d, today).is_empty());
        append(&path, "rolled-back\n");
        assert_eq!(tailer.poll(&d, today), vec!["rolled-back"]);

        // A day change does NOT switch files for Latest logs.
        append(&path, "later\n");
        assert_eq!(tailer.poll(&d, day(2026, 7, 27)), vec!["later"]);
    }

    #[test]
    fn history_files_dated_sorted() {
        let (_tmp, d) = dirs();
        append(&d.diagnostics.join("service.log.2026-07-26"), "newer\n");
        append(&d.diagnostics.join("service.log.2026-07-25"), "older\n");
        assert_eq!(
            history_files(&d, LogFile::Service),
            vec![
                d.diagnostics.join("service.log.2026-07-25"),
                d.diagnostics.join("service.log.2026-07-26"),
            ]
        );
    }

    #[test]
    fn history_files_absent_is_empty() {
        let (_tmp, d) = dirs();
        assert!(history_files(&d, LogFile::Widget).is_empty());
        assert!(history_files(&d, LogFile::Install).is_empty());
    }
}
