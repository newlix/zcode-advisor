// Claude Code CLI backend: the advisor is a one-shot `claude -p` headless
// call — prompt (question + context) on stdin, plain text on stdout, the
// advisor system prompt via --system-prompt. Auth rides on the CLI's own
// login; no API key involved. Hygiene flags keep it a pure model call:
// no tools (--restricted), no user/project settings or MCP servers
// (--setting-sources "" --strict-mcp-config), no skills
// (--disable-slash-commands), no session files (--no-session-persistence).
//
// Timeout semantics mirror http.rs's deadline: poll try_wait and kill on
// expiry. Pipe discipline: the prompt can exceed the 64KB pipe buffer (the
// 48K-char conversation tail is up to ~192KB in UTF-8), so stdin is drained
// by its own writer thread and stdout/stderr by reader threads — waiting on
// the child before draining stdout would deadlock past the buffer size.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const STDOUT_CAP: usize = 1 << 20; // same cap as the HTTP path
pub const STDERR_CAP: usize = 64 << 10;
const POLL_INTERVAL: Duration = Duration::from_millis(50);
// grace period for reader/writer threads to finish after the child exits.
// Bounded on purpose: a descendant of claude inheriting the pipes can keep
// them open past the child's exit, and an unbounded join would wedge the
// consult lock (and every future consult) forever.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

// Fixed hygiene flag set shared by every invocation (tests drive `run` with
// other binaries, so this lives in one place).
pub fn cli_args(model: &str, system_prompt: &str) -> Vec<String> {
    let mut args: Vec<String> = [
        "-p",
        "--output-format",
        "text",
        "--no-session-persistence",
        "--restricted",
        "--disable-slash-commands",
        "--strict-mcp-config",
        "--setting-sources",
        // empty = load no setting sources at all. Caveat: on Windows an
        // npm-installed claude is a .cmd shim, and cmd.exe's argument parsing
        // can drop an empty argument — the flag then degrades to defaults
        // (unix behavior is verified); there is no valid non-empty "none"
        // value (`--setting-sources none` is rejected by the CLI).
        "",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.push("--system-prompt".to_string());
    args.push(system_prompt.to_string());
    if !model.trim().is_empty() {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    args
}

pub fn ask(bin: &str, model: &str, system_prompt: &str, prompt: &str, timeout: Duration) -> Result<String, String> {
    let resolved = resolve_bin(bin)?;
    crate::logger::info(&format!("claude spawn bin={}", resolved.display()));
    run(&resolved, &cli_args(model, system_prompt), prompt, timeout)
}

// run: spawn `bin args...`, feed prompt on stdin, capture stdout/stderr with
// caps, kill at the deadline. Parameterized enough for tests to drive with
// /bin/cat or /bin/sleep.
fn run(bin: &Path, args: &[String], prompt: &str, timeout: Duration) -> Result<String, String> {
    let mut cmd = Command::new(bin);
    cmd.args(args);
    // Nested-session markers must not leak into the child: CLAUDE_SESSION_ID
    // is injected by ZCode (hooks path) and would confuse a claude child;
    // CLAUDECODE*/entrypoint mark a running Claude Code parent. Auth
    // (ANTHROPIC_API_KEY, credentials) is deliberately kept.
    for var in ["CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT", "CLAUDE_SESSION_ID"] {
        cmd.env_remove(var);
    }
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("spawning {} failed: {e}", bin.display()))?;

    let prompt = prompt.to_string();
    let mut stdin = child.stdin.take().expect("stdin piped above");
    let writer_done = Arc::new(AtomicBool::new(false));
    let writer_flag = Arc::clone(&writer_done);
    let writer = std::thread::spawn(move || {
        // A child that exits without reading stdin gives EPIPE here; the
        // failure surfaces via the exit status / stderr, so the write result
        // is ignored. stdin closed on drop → the child sees EOF.
        let _ = std::io::Write::write_all(&mut stdin, prompt.as_bytes());
        writer_flag.store(true, Ordering::SeqCst);
    });

    let stdout_buf = Arc::new(Mutex::new(Vec::new()));
    let stderr_buf = Arc::new(Mutex::new(Vec::new()));
    let mut done_flags = vec![writer_done];
    spawn_readers(&mut child, Arc::clone(&stdout_buf), Arc::clone(&stderr_buf), &mut done_flags);

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill(); // pipes close → readers drain and exit on their own
                    let _ = child.wait();
                    return Err(format!(
                        "claude CLI timed out after {timeout:?} (raise timeout_secs in the config file)"
                    ));
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(e) => return Err(format!("waiting for claude CLI: {e}")),
        }
    };
    // The child is gone, so the pipes usually hit EOF immediately. The wait is
    // bounded: if a descendant holds a pipe open, we proceed with whatever was
    // buffered rather than wedging the consult lock.
    let drained = wait_until_done(&done_flags, DRAIN_GRACE);
    let _ = writer; // detached if it didn't finish within the grace period

    let stdout = String::from_utf8_lossy(&stdout_buf.lock().expect("reader thread panicked")).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_buf.lock().expect("reader thread panicked")).into_owned();

    if !status.success() {
        let detail = match status.code() {
            Some(c) => format!("exit code {c}"),
            None => "killed by signal".to_string(),
        };
        return Err(format!("claude CLI failed ({detail}): {}", crate::util::truncate(stderr.trim(), 500)));
    }
    let text = stdout.trim();
    if text.is_empty() {
        let mut hint = if stderr.trim().is_empty() {
            String::new()
        } else {
            format!(" (stderr: {})", crate::util::truncate(stderr.trim(), 500))
        };
        if !drained {
            hint.push_str("; output may be incomplete (a child process kept the pipes open past the drain window)");
        }
        return Err(format!("claude CLI returned an empty response{hint}"));
    }
    Ok(text.to_string())
}

fn spawn_readers(
    child: &mut std::process::Child,
    stdout_buf: Arc<Mutex<Vec<u8>>>,
    stderr_buf: Arc<Mutex<Vec<u8>>>,
    done_flags: &mut Vec<Arc<AtomicBool>>,
) {
    if let Some(out) = child.stdout.take() {
        let buf = Arc::clone(&stdout_buf);
        let done = Arc::new(AtomicBool::new(false));
        done_flags.push(Arc::clone(&done));
        std::thread::spawn(move || read_capped(out, buf, done, STDOUT_CAP));
    }
    if let Some(err) = child.stderr.take() {
        let buf = Arc::clone(&stderr_buf);
        let done = Arc::new(AtomicBool::new(false));
        done_flags.push(Arc::clone(&done));
        std::thread::spawn(move || read_capped(err, buf, done, STDERR_CAP));
    }
}

// wait_until_done: poll the flags until all are set or the budget runs out;
// false = some thread missed the budget (pipes still held — output may be
// incomplete). Threads that miss stay detached — they only ever block on pipe
// IO and exit whenever the last pipe holder disappears; nothing joins them.
fn wait_until_done(flags: &[Arc<AtomicBool>], budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        if flags.iter().all(|f| f.load(Ordering::SeqCst)) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

// read_capped: buffer up to `cap` bytes, then keep draining (discarding) to
// EOF — a child that wrote past the cap must not block on a full pipe, or it
// would run into the deadline kill with a perfectly good (oversized) response.
fn read_capped<R: Read + Send + 'static>(mut reader: R, buf: Arc<Mutex<Vec<u8>>>, done: Arc<AtomicBool>, cap: usize) {
    let mut tmp = [0u8; 8192];
    let mut total = 0usize;
    loop {
        match reader.read(&mut tmp) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if total < cap {
                    let keep = n.min(cap - total);
                    if let Ok(mut b) = buf.lock() {
                        b.extend_from_slice(&tmp[..keep]);
                    }
                }
                total += n;
            }
        }
    }
    done.store(true, Ordering::SeqCst);
}

// resolve_bin turns the configured bin into an absolute path so the resolved
// location can be logged: a bare name is searched on PATH (the MCP server's
// PATH may differ from a login shell's — e.g. fnm's per-session dirs), with
// the native installer's usual locations as fallback.
pub fn resolve_bin(bin: &str) -> Result<PathBuf, String> {
    let direct = Path::new(bin);
    if direct.is_absolute() || bin.contains('/') || bin.contains('\\') {
        if is_executable_file(direct) {
            return Ok(direct.to_path_buf());
        }
        return Err(format!("configured claude bin is not an executable file: {bin}"));
    }
    if let Some(p) = search_path(bin) {
        return Ok(p);
    }
    let fallbacks = [
        util_home().join(".local").join("bin").join(bin),
        PathBuf::from("/usr/local/bin").join(bin),
    ];
    for f in &fallbacks {
        if is_executable_file(f) {
            return Ok(f.clone());
        }
    }
    Err(format!(
        "claude CLI not found: no '{bin}' on PATH, nor at {} — is the Claude Code CLI installed and logged in?",
        fallbacks.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
    ))
}

fn util_home() -> PathBuf {
    crate::util::home_dir()
}

fn search_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let suffixes: &[&str] = if cfg!(windows) { &["", ".exe", ".cmd", ".bat"] } else { &[""] };
    for dir in std::env::split_paths(&path) {
        for sfx in suffixes {
            let cand = dir.join(format!("{name}{sfx}"));
            if is_executable_file(&cand) {
                return Some(cand);
            }
        }
    }
    None
}

#[cfg(unix)]
fn is_executable_file(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(p: &Path) -> bool {
    std::fs::metadata(p).map(|m| m.is_file()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn oversized_stdout_is_truncated_not_killed() {
        // a child writing past STDOUT_CAP must not block on a full pipe and
        // run into the deadline kill — read_capped drains past the cap
        let out = run(
            Path::new("/bin/sh"),
            &s(&["-c", "head -c 2000000 /dev/zero | tr '\\0' 'x'; echo TAIL"]),
            "",
            Duration::from_secs(20),
        )
        .unwrap();
        assert_eq!(out.len(), STDOUT_CAP, "output must be capped at the buffer");
        assert!(!out.contains("TAIL"), "excess bytes are discarded");
    }

    #[test]
    fn cli_args_shape() {
        let a = cli_args("", "SP");
        assert!(a.windows(2).any(|w| w[0] == "--system-prompt" && w[1] == "SP"));
        assert!(!a.contains(&"--model".to_string()));
        let a = cli_args("sonnet", "SP");
        assert!(a.windows(2).any(|w| w[0] == "--model" && w[1] == "sonnet"));
        // hygiene flags present exactly once each
        for flag in ["--restricted", "--no-session-persistence", "--disable-slash-commands", "--strict-mcp-config"] {
            assert_eq!(a.iter().filter(|x| *x == flag).count(), 1, "{flag}");
        }
    }

    #[test]
    fn echoes_stdin_back() {
        // /bin/cat has no idea about claude flags — run() is the test seam
        let out = run(Path::new("/bin/cat"), &[], "hello advisor", Duration::from_secs(10)).unwrap();
        assert_eq!(out, "hello advisor");
    }

    #[test]
    fn deadline_kills_the_child() {
        let started = Instant::now();
        let err = run(Path::new("/bin/sleep"), &s(&["5"]), "", Duration::from_millis(400)).unwrap_err();
        assert!(err.contains("timed out"), "unexpected error: {err}");
        let elapsed = started.elapsed();
        assert!(elapsed < Duration::from_secs(4), "kill took too long: {elapsed:?}");
    }

    #[test]
    fn nonzero_exit_surfaces_stderr() {
        let err = run(
            Path::new("/bin/sh"),
            &s(&["-c", "echo boom >&2; exit 3"]),
            "",
            Duration::from_secs(10),
        )
        .unwrap_err();
        assert!(err.contains("exit code 3"), "unexpected error: {err}");
        assert!(err.contains("boom"), "stderr lost: {err}");
    }

    #[test]
    fn env_scrub_hides_nested_session_markers() {
        std::env::set_var("CLAUDE_SESSION_ID", "sess_leak");
        std::env::set_var("CLAUDECODE", "1");
        let out = run(
            Path::new("/bin/sh"),
            &s(&["-c", "echo \"sid=[$CLAUDE_SESSION_ID] cc=[$CLAUDECODE]\""]),
            "",
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(out, "sid=[] cc=[]");
    }

    #[test]
    fn resolve_bin_absolute_and_missing() {
        assert_eq!(resolve_bin("/bin/cat").unwrap(), PathBuf::from("/bin/cat"));
        let err = resolve_bin("/definitely/not/here").unwrap_err();
        assert!(err.contains("not an executable file"), "{err}");
        // bare name that exists nowhere → named-candidates error
        let err = resolve_bin("zca-no-such-binary").unwrap_err();
        assert!(err.contains("not found"), "{err}");
    }
}
