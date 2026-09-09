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
// other binaries, so this lives in one place). fallback_model rides the CLI's
// native --fallback-model (automatic model switch when the primary is
// overloaded/unavailable); suppressed when empty or equal to the model — the
// CLI rejects a fallback equal to the main model.
fn base_args(model: &str, fallback_model: &str, system_prompt: &str) -> Vec<String> {
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
    let fb = fallback_model.trim();
    if !fb.is_empty() && fb != model.trim() {
        args.push("--fallback-model".to_string());
        args.push(fb.to_string());
    }
    args
}

// cli_args: the advisor call — pure model call, no tools.
pub fn cli_args(model: &str, fallback_model: &str, system_prompt: &str) -> Vec<String> {
    base_args(model, fallback_model, system_prompt)
}

// reviewer_args: the review call — an agentic one-shot. --restricted keeps
// Bash/PowerShell/REPL removed and confines file access to the workspace;
// --allowedTools whitelists read-only tools (config-validated against
// {Read, Grep, Glob}); --add-dir extends the boundary to named directories.
pub fn reviewer_args(
    model: &str,
    fallback_model: &str,
    system_prompt: &str,
    tools: &str,
    add_dirs: &[String],
) -> Vec<String> {
    let mut args = base_args(model, fallback_model, system_prompt);
    args.push("--allowedTools".to_string());
    args.push(tools.to_string());
    for d in add_dirs {
        args.push("--add-dir".to_string());
        args.push(d.clone());
    }
    args
}

// ask: the advisor call. Returns (text, model that actually answered) — the
// quota fallback can make the fallback model the answerer, and the caller
// labels the advice with it. The primary attempt carries the native
// --fallback-model (the CLI's own overloaded/unavailable switch); the
// quota-retry attempt doesn't (the fallback became the primary).
pub fn ask(
    bin: &str,
    model: &str,
    fallback_model: &str,
    system_prompt: &str,
    prompt: &str,
    timeout: Duration,
) -> Result<(String, String), String> {
    let resolved = resolve_bin(bin)?;
    with_quota_fallback(model, fallback_model, |m| {
        let native_fb = if m == model { fallback_model } else { "" };
        crate::logger::info(&format!("claude spawn bin={} model={m}", resolved.display()));
        run(&resolved, &cli_args(m, native_fb, system_prompt), prompt, timeout)
    })
}

pub fn ask_review(
    bin: &str,
    model: &str,
    fallback_model: &str,
    system_prompt: &str,
    tools: &str,
    add_dirs: &[String],
    prompt: &str,
    timeout: Duration,
) -> Result<(String, String), String> {
    let resolved = resolve_bin(bin)?;
    with_quota_fallback(model, fallback_model, |m| {
        let native_fb = if m == model { fallback_model } else { "" };
        crate::logger::info(&format!(
            "claude spawn (review) bin={} model={m} tools={tools} add_dirs={}",
            resolved.display(),
            add_dirs.join(",")
        ));
        run(&resolved, &reviewer_args(m, native_fb, system_prompt, tools, add_dirs), prompt, timeout)
    })
}

// is_quota_error: usage-limit/credits exhaustion — the trigger for the
// one-shot retry with the fallback model. Wordings verified in the CLI
// binary (2.1.266): "usage limit reached", "You're out of usage credits",
// "credit balance too low", "You've hit your monthly spend limit", "You've
// hit your fast limit". Deliberately narrow in other directions: transient
// failures (rate limited, overloaded, 5xx) are the native --fallback-model's
// documented territory, and timeouts never count (an answer *discussing*
// usage limits that runs out of clock must not buy a second full budget —
// see is_timeout_error).
pub fn is_quota_error(msg: &str) -> bool {
    let m = msg.to_lowercase();
    m.contains("usage limit")
        || m.contains("usage credit")
        || m.contains("credit balance")
        || m.contains("spend limit")
        || m.contains("hit your")
}

// is_timeout_error: deadline kills are never quota-retried. The marker text
// is ours (run()'s timeout message); the classifier only ever sees strings
// this module built, so matching it stays contained.
fn is_timeout_error(msg: &str) -> bool {
    msg.contains("claude CLI timed out after")
}

// with_quota_fallback: run `attempt` on the primary model; on a
// quota-classified failure with a distinct fallback configured, retry once
// with the fallback as primary. Returns (text, model that answered).
//   primary quota + fallback quota → "quota exhausted" naming both models
//   primary quota + fallback other error → the fallback's error (actionable)
//   primary non-quota error or deadline kill → returned as-is, no retry
//   (a killed call whose output merely mentions usage limits is a long
//   answer, not a quota notice — a retry would double the wait)
// The wording is role-neutral: both the advisor and the reviewer route
// through here.
fn with_quota_fallback<F>(primary: &str, fallback: &str, attempt: F) -> Result<(String, String), String>
where
    F: Fn(&str) -> Result<String, String>,
{
    match attempt(primary) {
        Ok(text) => Ok((text, primary.to_string())),
        Err(e) => {
            let retryable =
                is_quota_error(&e) && !is_timeout_error(&e) && !fallback.trim().is_empty() && fallback.trim() != primary.trim();
            if !retryable {
                return Err(e);
            }
            crate::logger::info(&format!("quota retry primary={primary} fallback={fallback}"));
            match attempt(fallback) {
                Ok(text) => Ok((text, fallback.to_string())),
                Err(e2) if is_quota_error(&e2) => Err(format!(
                    "quota exhausted: both {primary} and {fallback} are over their usage limit (last error: {})",
                    crate::util::truncate(&e2, 300)
                )),
                Err(e2) => Err(format!("model {primary} hit its quota; fallback {fallback} then failed: {e2}")),
            }
        }
    }
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
                    // bounded drain so the tails include what the child wrote
                    // around the kill (quota notices may be all we have to
                    // classify a hung call with)
                    let _ = wait_until_done(&done_flags, DRAIN_GRACE);
                    let tails = stream_tails(&stdout_buf, &stderr_buf);
                    return Err(format!(
                        "claude CLI timed out after {timeout:?} (raise timeout_secs in the config file){tails}"
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
        // headless text mode puts terminal API errors (usage limits, auth
        // failures, bad models) on stdout and exits 1; stderr carries only
        // warnings — so stdout leads the message and stderr is appended.
        // Tails are real suffixes (last_chars): the notice is the last thing
        // the CLI prints.
        let out = stdout.trim();
        let mut msg = format!("claude CLI failed ({detail})");
        if !out.is_empty() {
            msg.push_str(&format!(": {}", last_chars(out, 500)));
        }
        let err = stderr.trim();
        if !err.is_empty() {
            msg.push_str(&format!(" (stderr: {})", last_chars(err, 500)));
        }
        return Err(msg);
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

// last_chars: char-boundary-safe SUFFIX of s (util::truncate keeps the head;
// error/notice text lands at the end of a stream, so tails must come from
// the end).
fn last_chars(s: &str, n: usize) -> &str {
    if s.len() <= n {
        return s;
    }
    let mut cut = s.len() - n;
    while !s.is_char_boundary(cut) {
        cut += 1;
    }
    &s[cut..]
}

// stream_tails: last words of both pipes as an error-message suffix — the
// only diagnostic we have for a call we killed at the deadline.
fn stream_tails(stdout_buf: &Arc<Mutex<Vec<u8>>>, stderr_buf: &Arc<Mutex<Vec<u8>>>) -> String {
    let stdout = String::from_utf8_lossy(&stdout_buf.lock().expect("reader thread panicked")).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_buf.lock().expect("reader thread panicked")).into_owned();
    let mut out = String::new();
    let s = stdout.trim();
    if !s.is_empty() {
        out.push_str(&format!(" (last stdout: {})", last_chars(s, 500)));
    }
    let s = stderr.trim();
    if !s.is_empty() {
        out.push_str(&format!(" (last stderr: {})", last_chars(s, 500)));
    }
    out
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
        let a = cli_args("", "", "SP");
        assert!(a.windows(2).any(|w| w[0] == "--system-prompt" && w[1] == "SP"));
        assert!(!a.contains(&"--model".to_string()));
        assert!(!a.contains(&"--fallback-model".to_string()));
        let a = cli_args("sonnet", "", "SP");
        assert!(a.windows(2).any(|w| w[0] == "--model" && w[1] == "sonnet"));
        // fallback flag only when distinct from the model (the CLI rejects
        // same-model fallbacks)
        let a = cli_args("fable", "opus", "SP");
        assert!(a.windows(2).any(|w| w[0] == "--fallback-model" && w[1] == "opus"));
        let a = cli_args("opus", "opus", "SP");
        assert!(!a.contains(&"--fallback-model".to_string()));
        // hygiene flags present exactly once each
        for flag in ["--restricted", "--no-session-persistence", "--disable-slash-commands", "--strict-mcp-config"] {
            assert_eq!(a.iter().filter(|x| *x == flag).count(), 1, "{flag}");
        }
        // the advisor call must NOT carry tool grants
        assert!(!a.contains(&"--allowedTools".to_string()));
        assert!(!a.contains(&"--add-dir".to_string()));
    }

    #[test]
    fn reviewer_args_shape() {
        let a = reviewer_args("opus", "", "RP", "Read,Grep,Glob", &["/w1".to_string(), "/w2".to_string()]);
        assert!(a.windows(2).any(|w| w[0] == "--allowedTools" && w[1] == "Read,Grep,Glob"));
        assert!(a.windows(2).any(|w| w[0] == "--add-dir" && w[1] == "/w1"));
        assert!(a.windows(2).any(|w| w[0] == "--add-dir" && w[1] == "/w2"));
        assert!(a.windows(2).any(|w| w[0] == "--system-prompt" && w[1] == "RP"));
        assert!(a.windows(2).any(|w| w[0] == "--model" && w[1] == "opus"));
        // defense in depth: --restricted stays on even with tools granted
        assert!(a.contains(&"--restricted".to_string()));
        // no add_dirs → no dangling --add-dir
        let a = reviewer_args("", "", "RP", "Read", &[]);
        assert!(!a.contains(&"--add-dir".to_string()));
        assert!(a.windows(2).any(|w| w[0] == "--allowedTools" && w[1] == "Read"));
        let a = reviewer_args("fable", "opus", "RP", "Read", &[]);
        assert!(a.windows(2).any(|w| w[0] == "--fallback-model" && w[1] == "opus"));
    }

    // ---- quota fallback ----

    #[test]
    fn quota_classifier_matches_limit_and_credit_wording() {
        // wordings verified in the CLI binary (2.1.266)
        assert!(is_quota_error("claude CLI failed (exit code 1): You're out of usage credits"));
        assert!(is_quota_error("Usage limit reached — check plan"));
        assert!(is_quota_error("Fable 5 requires usage credits"));
        assert!(is_quota_error("your credit balance is too low"));
        assert!(is_quota_error("claude CLI failed (exit code 1): You've hit your monthly spend limit."));
        assert!(is_quota_error("You've hit your fast limit"));
        assert!(!is_quota_error("claude CLI failed (exit code 1): 401 API key is invalid"));
        assert!(!is_quota_error("rate limited — wait and retry"));
        assert!(!is_quota_error("claude CLI timed out after 600s"));
        assert!(!is_quota_error(""));
    }

    #[test]
    fn timeout_errors_never_count_as_quota() {
        // a killed call whose captured output mentions usage limits is a long
        // answer, not a quota notice — the retry must not fire
        let n = std::sync::atomic::AtomicUsize::new(0);
        let err = with_quota_fallback("fable", "opus", |m| {
            n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(format!(
                "claude CLI timed out after 600s (raise timeout_secs in the config file) (last stdout: a review discussing the {m} usage limit)"
            ))
        })
        .unwrap_err();
        assert_eq!(n.load(std::sync::atomic::Ordering::SeqCst), 1, "no retry after a deadline kill");
        assert!(err.contains("timed out"), "{err}");
    }

    #[test]
    fn last_chars_keeps_the_suffix_on_char_boundaries() {
        assert_eq!(last_chars("short", 10), "short");
        assert_eq!(last_chars("0123456789", 4), "6789");
        let s = "é".repeat(10); // 2 bytes each
        assert_eq!(last_chars(&s, 6), "ééé");
        assert_eq!(last_chars("", 5), "");
    }

    #[test]
    fn quota_fallback_paths() {
        let calls = std::cell::RefCell::new(Vec::new());
        let attempt = |m: &str| {
            calls.borrow_mut().push(m.to_string());
            match m {
                "fable" => Err("claude CLI failed (exit code 1): You're out of usage credits".to_string()),
                _ => Ok(format!("answered by {m}")),
            }
        };
        // quota on primary → one retry, fallback answers
        let (text, model) = with_quota_fallback("fable", "opus", attempt).unwrap();
        assert_eq!(text, "answered by opus");
        assert_eq!(model, "opus");
        assert_eq!(*calls.borrow(), vec!["fable".to_string(), "opus".to_string()]);

        // both quota → exhausted report naming both models
        let text = with_quota_fallback("fable", "opus", |m| {
            Err(format!("claude CLI failed (exit code 1): {m} usage limit reached"))
        })
        .unwrap_err();
        assert!(text.contains("quota exhausted") && text.contains("fable") && text.contains("opus"), "{text}");

        // non-quota error → returned as-is, no retry
        let n = std::sync::atomic::AtomicUsize::new(0);
        let err = with_quota_fallback("fable", "opus", |_| {
            n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err("claude CLI failed (exit code 1): 401 API key is invalid".to_string())
        })
        .unwrap_err();
        assert_eq!(n.load(std::sync::atomic::Ordering::SeqCst), 1, "no retry on non-quota errors");
        assert!(err.contains("401"), "{err}");

        // no fallback configured / fallback == primary → single attempt
        let n = std::sync::atomic::AtomicUsize::new(0);
        let err = with_quota_fallback("fable", "", |_| {
            n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err("usage limit reached".to_string())
        })
        .unwrap_err();
        assert_eq!(n.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(err.contains("usage limit"), "{err}");
        let n = std::sync::atomic::AtomicUsize::new(0);
        let _ = with_quota_fallback("opus", "opus", |_| {
            n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err("usage limit reached".to_string())
        });
        assert_eq!(n.load(std::sync::atomic::Ordering::SeqCst), 1);

        // quota on primary, non-quota on fallback → fallback's error carries
        let err = with_quota_fallback("fable", "opus", |m| match m {
            "fable" => Err("usage limit reached".to_string()),
            _ => Err("network unreachable".to_string()),
        })
        .unwrap_err();
        assert!(err.contains("hit its quota") && err.contains("network unreachable"), "{err}");
    }

    #[test]
    fn end_to_end_quota_fallback_via_fake_cli() {
        // fake CLI shaped like the real thing (verified against 2.1.266):
        // terminal errors print to stdout and exit 1
        let dir = std::env::temp_dir().join(format!("zca-fake-cli-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake-claude.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\nprev=\"\"\nfor a in \"$@\"; do\n  if [ \"$prev\" = \"--model\" ]; then M=\"$a\"; fi\n  prev=\"$a\"\ndone\nif [ \"$M\" = \"fable\" ]; then\n  echo \"You're out of usage credits\"\n  exit 1\nfi\necho \"ANSWERED_BY=$M\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let bin = script.to_str().unwrap();
        let (text, model) = ask(bin, "fable", "opus", "SP", "prompt", Duration::from_secs(10)).unwrap();
        assert_eq!(model, "opus");
        assert_eq!(text, "ANSWERED_BY=opus");
        // the native --fallback-model rides only the primary attempt
        let args = cli_args("fable", "opus", "SP");
        let idx = args.iter().position(|x| x == "--fallback-model").unwrap();
        assert_eq!(args[idx + 1], "opus");
        let _ = std::fs::remove_file(&script);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn timeout_error_carries_quota_tails_but_never_retries() {
        // a hung child that printed quota text before sleeping: the deadline
        // kill surfaces the text for diagnosis, but a timeout never triggers
        // the fallback retry (the text could be an answer, not a notice)
        let err = run(
            Path::new("/bin/sh"),
            &s(&["-c", "echo 'Usage limit reached — resets at 15:00'; sleep 5"]),
            "",
            Duration::from_millis(400),
        )
        .unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        assert!(err.contains("Usage limit reached"), "quota tail lost: {err}");
        assert!(is_timeout_error(&err), "timeout marker missing");
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
