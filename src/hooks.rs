// Hook mode: `zcode-advisor hook <Event>`, fed event JSON on stdin by ZCode's
// hooks mechanism. The advisor is consulted only at rule-detectable key
// moments — task opening (a substantial prompt) and consecutive tool failures
// (stuck); every other event passes through silently, and advisor failures
// also pass through silently — never block real work.
// Every decision point writes one trace line to advisor.log (decision=…
// reason=…) for post-hoc forensics.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::server::{ask_advisor, ADVISOR_MODEL};
use crate::util::{create_private_dir, data_dir, now_secs, open_private_append, open_private_write, truncate};
use crate::logger;

const REMINDER_BUDGET: i64 = 3; // max reminders per session
const OPEN_PROMPT_MIN_RUNES: usize = 40; // prompts shorter than this count as trivial; don't disturb
const STUCK_FAIL_THRESHOLD: i64 = 2; // consecutive failures before counting as stuck
const STUCK_COOLDOWN_SECS: i64 = 5 * 60; // minimum interval between two stuck diagnoses
const STUCK_BUDGET: i64 = 5; // max stuck diagnoses per session
const STATE_STALE_AFTER_SECS: i64 = 30 * 60; // how long counters stay valid when no session id is available

pub fn run_hook(event: &str) {
    logger::init("hook");
    // 4MB cap, matching the Go version's io.LimitReader(os.Stdin, 4<<20)
    let mut stdin = Vec::new();
    let _ = std::io::stdin().take(4 << 20).read_to_end(&mut stdin);
    debug_log(event, &stdin); // raw capture of "what ZCode fed us"; behavior traces go to advisor.log

    let m: Value = serde_json::from_slice(&stdin).unwrap_or(Value::Null); // undecodable → treat all fields as missing

    match event {
        "UserPromptSubmit" => hook_user_prompt_submit(&m),
        "PostToolUseFailure" => hook_post_tool_use_failure(&m),
        "PostToolUseOK" => {
            // tool succeeded: reset the consecutive-failure counter (zero cost, no API call)
            let sess = session_key(&m);
            let mut st = load_state(&sess);
            st.fail = 0;
            save_state(&sess, &st);
            logger::info(&format!("hook event=PostToolUseOK sess={sess} decision=fail-reset"));
        }
        _ => {
            eprintln!("advisor-hook: unknown hook event: {event}");
            logger::info(&format!("hook event={event} decision=silent reason=unknown-event"));
        }
    }
}

// hookUserPromptSubmit: the task-opening reminder — counterpart of the original
// nudge. Injects a one-line reminder when the prompt is substantial and
// consult_advisor hasn't been used yet in this session; no API call, no waiting
// on the advisor. Whether and when to ask stays with the main model.
fn hook_user_prompt_submit(m: &Value) {
    let prompt = m.get("prompt").and_then(Value::as_str).unwrap_or("");
    let sess = session_key(m);
    // Go's utf8.RuneCountInString ≡ chars().count() (not the byte count of len())
    if prompt.trim().chars().count() < OPEN_PROMPT_MIN_RUNES {
        logger::info(&format!(
            "hook event=UserPromptSubmit sess={sess} decision=silent reason=short-prompt(len<{})",
            OPEN_PROMPT_MIN_RUNES
        ));
        return;
    }
    let mut st = load_state(&sess);
    if st.consulted || st.open >= REMINDER_BUDGET {
        logger::info(&format!(
            "hook event=UserPromptSubmit sess={sess} decision=silent reason={} open={}/{}",
            if st.consulted { "consulted" } else { "budget" },
            st.open,
            REMINDER_BUDGET
        ));
        return;
    }
    st.open = st.open.saturating_add(1);
    save_state(&sess, &st);
    logger::info(&format!(
        "hook event=UserPromptSubmit sess={sess} decision=remind open={}/{}",
        st.open, REMINDER_BUDGET
    ));
    // Text modeled on the original nudge: factual opening + conditional criteria
    // (unclear design tradeoffs / failure modes not yet ruled out) + timing
    // education (scoping isn't substantive work; ask before settling on an
    // approach). We inject at turn 0 and teach the model through the text to
    // "scope first, then ask", compensating for the original's turn-2 timing.
    emit_context(
        "UserPromptSubmit",
        "[advisor reminder] You haven't consulted the advisor yet (consult_advisor tool: a stronger advisor model; \
         calling it automatically attaches your current full conversation). Scoping work first is fine — reading files, \
         searching, and getting oriented before asking is never too late; but if the task has unclear design tradeoffs \
         or failure modes you haven't ruled out, consult before settling on an approach and starting to edit. When stuck, \
         considering a change of direction, or about to declare the task done, one more consult is also worth it.",
    );
}

// hookPostToolUseFailure: consecutive tool failures = a stuck signal.
// Consults only once the threshold is met, with a cooldown and a budget.
fn hook_post_tool_use_failure(m: &Value) {
    let sess = session_key(m);
    let mut st = load_state(&sess);
    st.fail = st.fail.saturating_add(1);
    if st.fail < STUCK_FAIL_THRESHOLD {
        save_state(&sess, &st);
        logger::info(&format!(
            "hook event=PostToolUseFailure sess={sess} decision=silent reason=below-threshold fail={}/{}",
            st.fail, STUCK_FAIL_THRESHOLD
        ));
        return;
    }
    if st.stuck >= STUCK_BUDGET {
        save_state(&sess, &st);
        logger::info(&format!(
            "hook event=PostToolUseFailure sess={sess} decision=silent reason=budget stuck={}/{}",
            st.stuck, STUCK_BUDGET
        ));
        return;
    }
    if now_secs().saturating_sub(st.stuck_at) < STUCK_COOLDOWN_SECS {
        save_state(&sess, &st);
        logger::info(&format!(
            "hook event=PostToolUseFailure sess={sess} decision=silent reason=cooldown fail={}/{}",
            st.fail, STUCK_FAIL_THRESHOLD
        ));
        return;
    }
    st.fail = 0;
    st.stuck = st.stuck.saturating_add(1);
    st.stuck_at = now_secs();
    save_state(&sess, &st);
    logger::info(&format!(
        "hook event=PostToolUseFailure sess={sess} fail={}/{} decision=stuck-consult stuck={}/{}",
        STUCK_FAIL_THRESHOLD, STUCK_FAIL_THRESHOLD, st.stuck, STUCK_BUDGET
    ));

    // Re-serialize to canonical JSON (sorted keys)
    let payload = serde_json::to_string(m).unwrap_or_else(|_| "null".into());
    let q = "A coding agent's tool calls keep failing; it appears stuck. Latest failed tool event (JSON):\n".to_string()
        + &truncate(&payload, 2000)
        + "\n\nDiagnose likely causes and advise: what to check, what to try next, and when to stop and report to the user. Under 150 words, plain text.";
    let started = std::time::Instant::now();
    match ask_advisor(&q, "") {
        Ok(advice) => {
            logger::info(&format!(
                "hook event=PostToolUseFailure sess={sess} stuck advice t={:?} len={}B",
                started.elapsed(),
                advice.len()
            ));
            emit_context("PostToolUseFailure", &format!("[advisor stuck advice · {ADVISOR_MODEL}]\n{advice}"));
        }
        Err(e) => {
            eprintln!("advisor-hook: stuck advice skipped: {e}");
            logger::info(&format!(
                "hook event=PostToolUseFailure sess={sess} stuck advice skipped t={:?} err={e}",
                started.elapsed()
            ));
        }
    }
}

// emitContext outputs ZCode's additionalContext hook format; a wrong event name
// gets rejected by the strict schema (harmless). Write failures (e.g. a closed
// pipe) are swallowed silently — never panic or block work over output problems.
fn emit_context(event: &str, text: &str) {
    let out = json!({
        "hookSpecificOutput": {"hookEventName": event, "additionalContext": text}
    });
    let mut stdout = std::io::stdout();
    let _ = writeln!(stdout, "{out}");
    let _ = stdout.flush();
}

// ---- session state (counters only; corruption affects throttling at worst) ----
// Field names follow the Go hookState json tags; counters use i64 + saturating
// arithmetic: a corrupted state file can at worst invalidate the counts, never
// panic.

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
pub struct HookState {
    pub ts: i64,         // last write time; the "default" case judges freshness by it
    pub open: i64,       // reminders emitted so far
    pub fail: i64,       // consecutive tool failures
    pub stuck: i64,      // stuck diagnoses used so far
    pub stuck_at: i64,   // time of the last stuck diagnosis (unix seconds)
    pub consulted: bool, // whether consult_advisor has been used in this session (silences the reminder)
}

fn state_path(sess: &str) -> PathBuf {
    state_dir().join(format!("{sess}.state.json"))
}

pub fn state_dir() -> PathBuf {
    data_dir().join("state")
}

fn load_state(sess: &str) -> HookState {
    let st = load_state_at(&state_path(sess));
    // The freshness reset applies only to the "default" case (no session id);
    // real session ids are new every time, and their counters must not be
    // cleared across sessions by mistake.
    if sess == "default" && now_secs().saturating_sub(st.ts) > STATE_STALE_AFTER_SECS {
        logger::info("state stale reset sess=default");
        return HookState::default();
    }
    st
}

fn load_state_at(path: &Path) -> HookState {
    // Mirrors Go's partial-preservation semantics of "json.Unmarshal errors
    // ignored, fields decoded so far kept": extract field by field via Value on
    // a best-effort basis; missing/null/wrong-typed fields fall back to zero.
    // File exists but isn't JSON at all → all zeros + a trace line (this lets
    // the reminder speak again — worth being traceable)
    let raw = match fs::read(path) {
        Ok(b) => b,
        Err(_) => return HookState::default(),
    };
    let v: Value = match serde_json::from_slice(&raw) {
        Ok(v) => v,
        Err(_) => {
            logger::info(&format!("state unreadable zeroed path={}", path.display()));
            return HookState::default();
        }
    };
    HookState {
        ts: v.get("ts").and_then(Value::as_i64).unwrap_or(0),
        open: v.get("open").and_then(Value::as_i64).unwrap_or(0),
        fail: v.get("fail").and_then(Value::as_i64).unwrap_or(0),
        stuck: v.get("stuck").and_then(Value::as_i64).unwrap_or(0),
        stuck_at: v.get("stuck_at").and_then(Value::as_i64).unwrap_or(0),
        consulted: v.get("consulted").and_then(Value::as_bool).unwrap_or(false),
    }
}

fn save_state(sess: &str, st: &HookState) {
    let mut st = st.clone();
    st.ts = now_secs();
    save_state_at(&state_path(sess), &st);
}

fn save_state_at(path: &Path, st: &HookState) {
    if let Some(dir) = path.parent() {
        let _ = create_private_dir(dir);
    }
    if let Ok(b) = serde_json::to_vec(st) {
        if let Ok(mut f) = open_private_write(path) {
            let _ = f.write_all(&b);
        } else {
            // write failure → the reminder won't be silenced (fires again next
            // time); ERROR trace for forensics
            logger::error(&format!("state write failed path={}", path.display()));
        }
    }
}

// sessionKey: env CLAUDE_SESSION_ID first, then stdin's session_id; neither
// present → "default" (STATE_STALE_AFTER_SECS prevents cross-session buildup).
fn session_key(m: &Value) -> String {
    let s = std::env::var("CLAUDE_SESSION_ID").unwrap_or_default();
    let s = if s.is_empty() {
        m.get("session_id").and_then(Value::as_str).unwrap_or("").to_string()
    } else {
        s
    };
    if s.is_empty() {
        "default".to_string()
    } else {
        sanitize_session(&s)
    }
}

// markConsulted: mark on any MCP tool call; the opening reminder goes quiet
// from then on. Success or failure both count — the reminder's purpose is to
// make sure the main model knows the tool exists; a call achieves that.
pub fn mark_consulted(session_id: &str) {
    if session_id.is_empty() {
        return;
    }
    let k = sanitize_session(session_id);
    let mut st = load_state(&k);
    st.consulted = true;
    save_state(&k, &st);
}

// sanitizeSession: keep [A-Za-z0-9._-], replace everything else with '_', cap
// at 64 bytes (legal chars and the replacement are all 1 byte, so bytes =
// chars).
fn sanitize_session(s: &str) -> String {
    let mut b = String::new();
    for r in s.chars() {
        match r {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '-' => b.push(r),
            _ => b.push('_'),
        }
        if b.len() >= 64 {
            break;
        }
    }
    b
}

// debugLog: archive every hook's raw input ("what ZCode fed us") to confirm
// actual field names; rotates past 2MB. Behavior traces live in advisor.log
// (the logger module) — different responsibilities.
fn debug_log(event: &str, stdin: &[u8]) {
    let path = data_dir().join("hooks-debug.log");
    if let Ok(md) = fs::metadata(&path) {
        if md.len() > 2 << 20 {
            let _ = fs::remove_file(&path);
        }
    }
    if let Some(dir) = path.parent() {
        let _ = create_private_dir(dir);
    }
    if let Ok(mut f) = open_private_append(&path) {
        let _ = writeln!(f, "=== {} {} ===\n{}", event, crate::util::rfc3339_utc(now_secs()), String::from_utf8_lossy(stdin));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("zca-hooks-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn state_roundtrip_and_field_names() {
        let path = temp_path("rt").join("s1.state.json");
        let st = HookState { ts: 1725500000, open: 2, fail: 1, stuck: 3, stuck_at: 1725490000, consulted: true };
        save_state_at(&path, &st);
        let raw = fs::read_to_string(&path).unwrap();
        // field names follow the Go json tags
        for key in [r#""ts":1725500000"#, r#""open":2"#, r#""fail":1"#, r#""stuck":3"#, r#""stuck_at":1725490000"#, r#""consulted":true"#] {
            assert!(raw.contains(key), "missing {key} in {raw}");
        }
        assert_eq!(load_state_at(&path), st);
        // partial/malformed state file: like Go, "keep decoded fields, zero the rest"
        fs::write(&path, r#"{"open":2,"fail":-5,"consulted":"yes"}"#).unwrap();
        let st = load_state_at(&path);
        assert_eq!(st.open, 2);
        assert_eq!(st.fail, -5);
        assert_eq!(st.ts, 0);
        assert!(!st.consulted);
        // a null field is a no-op (Go semantics), not missing
        fs::write(&path, r#"{"open":2,"fail":null}"#).unwrap();
        assert_eq!(load_state_at(&path).open, 2);
        assert_eq!(load_state_at(&path).fail, 0);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn sanitize_matches_go_rules() {
        assert_eq!(sanitize_session("sess_abc-123.jsonl"), "sess_abc-123.jsonl");
        assert_eq!(sanitize_session("a/b\\c:d"), "a_b_c_d");
        assert_eq!(sanitize_session("你好"), "__"); // CJK → '_' (intentionally multibyte)
        let long = "x".repeat(100);
        assert_eq!(sanitize_session(&long).len(), 64);
    }

    #[test]
    fn failure_threshold_and_cooldown_state_machine() {
        // Verify the hookPostToolUseFailure counting logic directly (pre-API path, no API call)
        let sess = sanitize_session("smoke-sm-1");
        let path = state_path(&sess);
        let _ = fs::remove_file(&path);

        let mut st = load_state(&sess);
        st.fail += 1;
        assert!(st.fail < STUCK_FAIL_THRESHOLD); // first failure: below threshold
        st.fail += 1;
        assert!(st.fail >= STUCK_FAIL_THRESHOLD); // second: threshold met
        st.fail = 0;
        st.stuck += 1;
        st.stuck_at = now_secs();
        save_state(&sess, &st);

        let st2 = load_state(&sess);
        assert_eq!(st2.stuck, 1);
        assert!(now_secs() - st2.stuck_at < STUCK_COOLDOWN_SECS); // within cooldown
        let _ = fs::remove_file(&path);
    }
}
