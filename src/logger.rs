// Behavior-trace log (advisor.log): division of labor with hooks-debug.log —
// that one records "what ZCode fed us" (raw stdin captures), this one records
// "what we did and why" (consult lifecycle, hook decision points). The purpose
// is post-hoc forensics: why the advisor answered the way it did, why a hook
// fired / didn't fire.
//
// Structural traces only (decisions, outcomes, timings, sizes), never content —
// "what the advisor actually saw" (the full question, the conversation, the
// advice text) is preserved natively by ZCode's rollout files
// (~/.zcode/cli/rollout/), permanently reproducible; the log doesn't duplicate
// that.
//
// Design constraints:
// - The logging system must never fail a task: any write error is silently
//   dropped (same philosophy as hooks-debug).
// - Multi-process concurrency (two MCP connections + one-shot hook
//   processes): O_APPEND + a single write_all per line (line length far below
//   page size) keeps lines whole; the file is opened and closed per line —
//   the rate is tiny (a line or two per consult/hook), no need to hold it.
// - stdout never leaves a trace (it's the MCP protocol channel); existing
//   stderr lines stay as they are — this module only writes the file.
// - No level switches, no environment variables: both levels (INFO/ERROR)
//   are severity markers, always on — a behavior trace should record
//   everything; content has its own home.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::util::{create_private_dir, now_secs, open_private_append, rfc3339_utc};

const ROTATE_BYTES: u64 = 2 << 20; // same threshold as hooks-debug.log

// Levels are severity markers only (for grepping out ERROR); output is not filtered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Error,
    Info,
}

impl Level {
    fn name(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Info => "INFO",
        }
    }
}

// The mode (server|hook) is fixed at process start and prefixes every line for
// cross-process alignment.
static MODE: OnceLock<&'static str> = OnceLock::new();

// init marks this process's mode (server or hook); without it, log() still
// works and mode reads "?".
pub fn init(mode: &'static str) {
    let _ = MODE.set(mode);
}

pub fn info(msg: &str) {
    log(Level::Info, msg);
}

pub fn error(msg: &str) {
    log(Level::Error, msg);
}

fn log(level: Level, msg: &str) {
    let mode = *MODE.get().unwrap_or(&"?");
    let line = format!(
        "{} pid={} mode={} {} {}\n",
        rfc3339_utc(now_secs()),
        std::process::id(),
        mode,
        level.name(),
        msg
    );
    // Failure philosophy: can't write → drop — a missing log must never hold
    // up real work
    let _ = write_line(&log_path(), line.as_bytes());
}

fn log_path() -> PathBuf {
    crate::util::data_dir().join("advisor.log")
}

// write_line: rotate past the threshold first, then append the whole line with
// a single O_APPEND write.
fn write_line(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Ok(md) = std::fs::metadata(path) {
        if md.len() > ROTATE_BYTES {
            rotate(path);
        }
    }
    if let Some(dir) = path.parent() {
        create_private_dir(dir)?;
    }
    let mut f = open_private_append(path)?;
    f.write_all(bytes)
}

// rotate: rename keeping one generation (advisor.log.1); if the rename fails
// (e.g. the file is held open), fall back to truncating and reopening. Known
// accepted race: when multiple processes cross the rotation threshold at the
// same instant, the later rename may clobber the just-moved generation (the
// old .1 is lost; the current file and line integrity are not) — the window
// is ns–µs wide and needs two processes rotating at the same instant;
// accepted as best-effort (an extension of the silent-failure philosophy).
fn rotate(path: &Path) {
    let mut prev = path.as_os_str().to_os_string();
    prev.push(".1");
    if std::fs::rename(path, Path::new(&prev)).is_err() {
        let _ = crate::util::open_private_write(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_log(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("zca-log-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        p
    }

    fn log_to(path: &Path, msg: &str) {
        write_line(path, format!("{}\n", msg).as_bytes()).unwrap();
    }

    #[test]
    fn appends_lines_to_same_file() {
        let dir = temp_log("append");
        let p = dir.join("advisor.log");
        log_to(&p, "line1");
        log_to(&p, "line2");
        let content = fs::read_to_string(&p).unwrap();
        assert_eq!(content, "line1\nline2\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotates_over_threshold_keeping_one_generation() {
        let dir = temp_log("rotate");
        let p = dir.join("advisor.log");
        log_to(&p, "first-line");
        // grow the file past the rotation threshold via append (simulating
        // long-term accumulation; fs::write truncates, so it can't be used)
        let junk = vec![b'x'; (ROTATE_BYTES + 1) as usize];
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            use std::io::Write as _;
            let mut f = fs::OpenOptions::new().append(true).mode(0o600).open(&p).unwrap();
            f.write_all(&junk).unwrap();
        }
        log_to(&p, "after-rotate");
        // the whole old file (first-line + junk) moves to .1; the new file
        // holds only after-rotate
        let rotated = fs::read(dir.join("advisor.log.1")).unwrap();
        assert!(rotated.starts_with(b"first-line\n"), "old content must survive in .1");
        assert!(rotated.len() > ROTATE_BYTES as usize);
        assert_eq!(fs::read_to_string(&p).unwrap(), "after-rotate\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_writers_do_not_corrupt_lines() {
        let dir = temp_log("conc");
        let p = dir.join("advisor.log");
        let writers: Vec<_> = (0..4)
            .map(|w| {
                let p = p.clone();
                std::thread::spawn(move || {
                    for i in 0..50 {
                        write_line(&p, format!("w{w}-i{i:03}\n").as_bytes()).unwrap();
                    }
                })
            })
            .collect();
        for h in writers {
            h.join().unwrap();
        }
        let content = fs::read_to_string(&p).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 200, "line count must be whole (no interleaved corruption)");
        let mut sorted = lines.clone();
        sorted.sort_unstable();
        let mut expect: Vec<String> = (0..4)
            .flat_map(|w| (0..50).map(move |i| format!("w{w}-i{i:03}")))
            .collect();
        expect.sort_unstable();
        assert_eq!(sorted, expect.iter().map(|s| s.as_str()).collect::<Vec<_>>());
        let _ = fs::remove_dir_all(&dir);
    }
}
