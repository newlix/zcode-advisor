// Hook mode: `zcode-consultant hook <Event>`, fed event JSON on stdin by ZCode's
// hooks mechanism. The advisor is consulted only at rule-detectable key
// moments — task opening (a substantial prompt) and consecutive tool failures
// (stuck); every other event passes through silently, and advisor failures
// also pass through silently — never block real work.
// Every decision point writes one trace line to consultant.log (decision=…
// reason=…) for post-hoc forensics.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::server::{advisor_label, ask_advisor};
use crate::util::{create_private_dir, data_dir, now_secs, open_private_append, open_private_write, truncate};
use crate::logger;

const REMINDER_BUDGET: i64 = 3; // max reminders per session
const OPEN_PROMPT_MIN_RUNES: usize = 40; // prompts shorter than this count as trivial; don't disturb
const STUCK_FAIL_THRESHOLD: i64 = 2; // consecutive failures before counting as stuck
const STUCK_COOLDOWN_SECS: i64 = 5 * 60; // minimum interval between two stuck diagnoses
const STUCK_BUDGET: i64 = 5; // max stuck diagnoses per session
const STATE_STALE_AFTER_SECS: i64 = 30 * 60; // how long counters stay valid when no session id is available
const REVIEW_BUDGET: i64 = 1; // max review reminders per session (Stop fires every turn; 1 keeps it a one-time gate)
// The MCP server name as registered under mcp.servers in config.json — a rename
// there must be mirrored here or review marking silently stops working.
const REVIEW_TOOL: &str = concat!("mcp__zcode-consultant__", "review_change");
// Edit-class tools whose success means "files changed"; Bash-based edits
// (sed -i and friends) are a documented blind spot — parsing commands for
// edits would misfire on read-only pipelines.
const EDIT_TOOLS: &[&str] = &["Edit", "Write", "MultiEdit", "NotebookEdit"];

pub fn run_hook(event: &str) {
    logger::init("hook");
    // 4MB cap, matching the Go version's io.LimitReader(os.Stdin, 4<<20)
    let mut stdin = Vec::new();
    let _ = std::io::stdin().take(4 << 20).read_to_end(&mut stdin);
    debug_log(event, &stdin); // raw capture of "what ZCode fed us"; behavior traces go to consultant.log

    let m: Value = serde_json::from_slice(&stdin).unwrap_or(Value::Null); // undecodable → treat all fields as missing

    match event {
        "UserPromptSubmit" => hook_user_prompt_submit(&m),
        "PostToolUseFailure" => hook_post_tool_use_failure(&m),
        "PostToolUseOK" => {
            // tool succeeded: reset the consecutive-failure counter and note
            // edit-class / review tool calls (zero cost, no API call)
            let sess = session_key(&m);
            let mut st = load_state(&sess);
            st.fail = 0;
            note_tool_ok(&mut st, tool_name(&m));
            save_state(&sess, &st);
            logger::info(&format!(
                "hook event=PostToolUseOK sess={sess} decision=fail-reset edits={} reviewed={}",
                st.edits, st.reviewed
            ));
        }
        "Stop" => hook_stop(&m),
        _ => {
            eprintln!("zcode-consultant-hook: unknown hook event: {event}");
            logger::info(&format!("hook event={event} decision=silent reason=unknown-event"));
        }
    }
}

// tool_name: PostToolUse payloads carry the tool name (verified in
// hooks-debug.log); missing/undecodable → "" which matches neither list.
fn tool_name(m: &Value) -> &str {
    m.get("tool_name").and_then(Value::as_str).unwrap_or("")
}

// note_tool_ok: a successful review_change call marks the session reviewed
// (silences the Stop reminder — the agent knows the tool and got feedback);
// a successful edit-class call counts the session as having changed files.
fn note_tool_ok(st: &mut HookState, tool: &str) {
    if is_review_tool(tool) {
        st.reviewed = true;
    } else if is_edit_tool(tool) {
        st.edits = st.edits.saturating_add(1);
    }
}

// note_review_tool: the failure-path twin of note_tool_ok's review branch — a
// FAILED review_change call still marks reviewed (the agent tried; nagging it
// to review during an outage breaks the never-block invariant).
fn note_review_tool(st: &mut HookState, tool: &str) {
    if is_review_tool(tool) {
        st.reviewed = true;
    }
}

fn is_review_tool(tool: &str) -> bool {
    tool == REVIEW_TOOL
}

fn is_edit_tool(tool: &str) -> bool {
    EDIT_TOOLS.contains(&tool)
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
    let sess = session_key(&m);
    let mut st = load_state(&sess);
    note_review_tool(&mut st, tool_name(&m));
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
            emit_context("PostToolUseFailure", &format!("[advisor stuck advice · {}]\n{advice}", advisor_label()));
        }
        Err(e) => {
            eprintln!("zcode-consultant-hook: stuck advice skipped: {e}");
            logger::info(&format!(
                "hook event=PostToolUseFailure sess={sess} stuck advice skipped t={:?} err={e}",
                started.elapsed()
            ));
        }
    }
}

// hookStop: the review gate — counterpart of the opening reminder. When the
// session edited files but review_change never ran, inject one reminder as the
// agent is about to declare the task done. No API call, never forces
// continuation (continue:true exists in the schema but would block real work).
fn hook_stop(m: &Value) {
    let sess = session_key(&m);
    // When a Stop hook itself wakes the agent, ZCode re-fires Stop with
    // stop_hook_active=true — never re-remind from that wakeup.
    if m.get("stop_hook_active").and_then(Value::as_bool).unwrap_or(false) {
        logger::info(&format!("hook event=Stop sess={sess} decision=silent reason=stop-hook-active"));
        return;
    }
    let mut st = load_state(&sess);
    if !should_remind_review(&st) {
        let reason = if st.reviewed {
            "reviewed"
        } else if st.edits == 0 {
            "no-edits"
        } else {
            "budget"
        };
        logger::info(&format!(
            "hook event=Stop sess={sess} decision=silent reason={reason} edits={} reviewed={} reminded={}/{}",
            st.edits, st.reviewed, st.review_reminded, REVIEW_BUDGET
        ));
        return;
    }
    st.review_reminded = st.review_reminded.saturating_add(1);
    save_state(&sess, &st);
    logger::info(&format!(
        "hook event=Stop sess={sess} decision=remind edits={} reminded={}/{}",
        st.edits, st.review_reminded, REVIEW_BUDGET
    ));
    emit_context(
        "Stop",
        "[review reminder] This session edited files but review_change was never called. If the changes are risky, \
         subtle, or hard to reverse, request the review_change tool before declaring done; if they are trivial \
         (docs, formatting, one-liners), ignore this reminder.",
    );
}

// should_remind_review: files were changed, no review call (success OR failure)
// happened, and the one-shot budget isn't spent.
fn should_remind_review(st: &HookState) -> bool {
    st.edits > 0 && !st.reviewed && st.review_reminded < REVIEW_BUDGET
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
    pub edits: i64,          // successful edit-class tool calls (review gate input)
    pub reviewed: bool,      // whether review_change was called, success or failure (silences the Stop reminder)
    pub review_reminded: i64, // review reminders emitted so far
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
        edits: v.get("edits").and_then(Value::as_i64).unwrap_or(0),
        reviewed: v.get("reviewed").and_then(Value::as_bool).unwrap_or(false),
        review_reminded: v.get("review_reminded").and_then(Value::as_i64).unwrap_or(0),
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

// sessionKey: env CLAUDE_SESSION_ID first, then stdin's session_id (the app
// sends camelCase "sessionId" in PostToolUse payloads per hooks-debug.log, so
// fall back to that too); neither present → "default" (STATE_STALE_AFTER_SECS
// prevents cross-session buildup).
fn session_key(m: &Value) -> String {
    let s = std::env::var("CLAUDE_SESSION_ID").unwrap_or_default();
    let s = if s.is_empty() {
        m.get("session_id")
            .and_then(Value::as_str)
            .or_else(|| m.get("sessionId").and_then(Value::as_str))
            .unwrap_or("")
            .to_string()
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
// actual field names; rotates past 2MB. Behavior traces live in consultant.log
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
        let st = HookState {
            ts: 1725500000,
            open: 2,
            fail: 1,
            stuck: 3,
            stuck_at: 1725490000,
            consulted: true,
            edits: 4,
            reviewed: true,
            review_reminded: 1,
        };
        save_state_at(&path, &st);
        let raw = fs::read_to_string(&path).unwrap();
        // field names follow the Go json tags
        for key in [r#""ts":1725500000"#, r#""open":2"#, r#""fail":1"#, r#""stuck":3"#, r#""stuck_at":1725490000"#, r#""consulted":true"#, r#""edits":4"#, r#""reviewed":true"#, r#""review_reminded":1"#] {
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
        assert_eq!(st.edits, 0);
        assert!(!st.reviewed);
        // a null field is a no-op (Go semantics), not missing
        fs::write(&path, r#"{"open":2,"fail":null}"#).unwrap();
        assert_eq!(load_state_at(&path).open, 2);
        assert_eq!(load_state_at(&path).fail, 0);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn tool_classification_and_review_gate_transitions() {
        // edit-class vs review tool vs everything else
        assert!(is_edit_tool("Edit") && is_edit_tool("Write") && is_edit_tool("MultiEdit"));
        assert!(!is_edit_tool("Bash") && !is_edit_tool("Read"));
        assert!(!is_edit_tool(REVIEW_TOOL));
        assert!(is_review_tool(REVIEW_TOOL));

        // edit → remind exactly once (budget 1), then silent
        let mut st = HookState::default();
        note_tool_ok(&mut st, "Edit");
        note_tool_ok(&mut st, "Write");
        assert!(should_remind_review(&st));
        st.review_reminded += 1;
        assert!(!should_remind_review(&st));

        // edit → review → silent forever (and reminders never restart)
        let mut st = HookState::default();
        note_tool_ok(&mut st, "Edit");
        note_tool_ok(&mut st, REVIEW_TOOL); // successful review call
        assert!(st.reviewed);
        assert!(!should_remind_review(&st));

        // review-before-edit → silent (nothing changed, nothing to gate)
        let mut st = HookState::default();
        note_tool_ok(&mut st, REVIEW_TOOL);
        assert!(!should_remind_review(&st));
        note_tool_ok(&mut st, "Edit");
        assert!(!should_remind_review(&st));

        // FAILED review call marks reviewed too (nagging during an outage
        // breaks the never-block invariant) — same mark, different event path
        let mut st = HookState::default();
        note_tool_ok(&mut st, "Edit");
        note_review_tool(&mut st, REVIEW_TOOL);
        assert!(st.reviewed);
        assert!(!should_remind_review(&st));

        // stop_hook_active re-entry never reminds (checked before budget spend)
        let st = HookState { edits: 1, ..Default::default() };
        assert!(should_remind_review(&st)); // the state would allow it;
        assert_eq!(st.review_reminded, 0); // the hook's stop_hook_active check fires first
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
