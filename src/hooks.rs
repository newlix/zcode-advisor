// hook 模式：zcode-advisor hook <Event>，由 ZCode 的 hooks 機制以 stdin 餵入事件 JSON。
// 只在規則抓得住的關鍵時刻諮詢顧問——任務開場（有份量的 prompt）與連續工具失敗（卡關），
// 其餘事件一律靜默放行；顧問失敗也靜默放行，絕不擋工作。
// 每個決策點都寫一行軌跡到 advisor.log（decision=… reason=…），供事後追查。

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::server::{ask_advisor, ADVISOR_MODEL};
use crate::util::{create_private_dir, data_dir, now_secs, open_private_append, open_private_write, truncate};
use crate::logger;

const REMINDER_BUDGET: i64 = 3; // 每個 session 最多提醒幾次
const OPEN_PROMPT_MIN_RUNES: usize = 40; // prompt 低於此長度視為瑣碎請求，不打擾
const STUCK_FAIL_THRESHOLD: i64 = 2; // 連續失敗幾次視為卡關
const STUCK_COOLDOWN_SECS: i64 = 5 * 60; // 兩次卡關診斷的最小間隔
const STUCK_BUDGET: i64 = 5; // 每個 session 最多幾次卡關診斷
const STATE_STALE_AFTER_SECS: i64 = 30 * 60; // 拿不到 session id 時的計數有效期

pub fn run_hook(event: &str) {
    logger::init("hook");
    // 上限 4MB，對應 Go 版 io.LimitReader(os.Stdin, 4<<20)
    let mut stdin = Vec::new();
    let _ = std::io::stdin().take(4 << 20).read_to_end(&mut stdin);
    debug_log(event, &stdin); // 「ZCode 餵了什麼」的原始擷取；行為軌跡見 advisor.log

    let m: Value = serde_json::from_slice(&stdin).unwrap_or(Value::Null); // 解不動 → 全部查無值

    match event {
        "UserPromptSubmit" => hook_user_prompt_submit(&m),
        "PostToolUseFailure" => hook_post_tool_use_failure(&m),
        "PostToolUseOK" => {
            // 工具成功：重置連續失敗計數（零成本，不打 API）
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

// hookUserPromptSubmit：任務開場提醒——原版 nudge 的對應物。prompt 有份量且本 session
// 尚未用過 consult_advisor 時注入一行提醒，不打 API、不等顧問；要不要問、何時問由主模型自己決定。
fn hook_user_prompt_submit(m: &Value) {
    let prompt = m.get("prompt").and_then(Value::as_str).unwrap_or("");
    let sess = session_key(m);
    // Go 的 utf8.RuneCountInString ≡ chars().count()（不是 bytes 的 len()）
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
    // 文字仿原版 nudge：事實開頭＋條件式判準（不明的設計取捨／未排除的失敗模式）＋
    // timing 教育（定位不算實質工作、定下做法前要問）。我們在 turn 0 注入，
    // 靠文字教模型「先定位再問」，補原版 turn-2 nudge 的時點優勢。
    emit_context(
        "UserPromptSubmit",
        "【advisor 提醒】你還沒諮詢過 advisor（consult_advisor 工具：更強的顧問模型，\
         呼叫時會自動附上你目前的完整對話）。定位工作可以先做——讀檔、搜尋、了解現況之後再問不遲；\
         但如果任務有不明的設計取捨、或你尚未排除的失敗模式，請在定下做法、開始修改之前諮詢。\
         卡住、考慮換方向、或自認完成時，也值得再問一次。",
    );
}

// hookPostToolUseFailure：連續工具失敗 = 卡關訊號。達門檻才諮詢，有冷卻與預算。
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

    // 重新序列化成規範 JSON（鍵排序）
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
            emit_context("PostToolUseFailure", &format!("【advisor 卡關建議 · {ADVISOR_MODEL}】\n{advice}"));
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

// emitContext 輸出 ZCode hook 的 additionalContext 格式；事件名錯了會被嚴格 schema 拒掉（無害）。
// 寫入失敗（如管線已關）靜默吞掉——絕不因輸出問題 panic 或擋工作。
fn emit_context(event: &str, text: &str) {
    let out = json!({
        "hookSpecificOutput": {"hookEventName": event, "additionalContext": text}
    });
    let mut stdout = std::io::stdout();
    let _ = writeln!(stdout, "{out}");
    let _ = stdout.flush();
}

// ---- session 狀態（計數用，壞了也只影響節流）----
// 欄位名沿用 Go 版 hookState 的 json tags；計數用 i64＋飽和運算：
// 被改壞的狀態檔最多讓計數失效，不該 panic。

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
pub struct HookState {
    pub ts: i64,         // 最後寫入時間；default 案例靠它判斷新鮮度
    pub open: i64,       // 提醒已發出次數
    pub fail: i64,       // 連續工具失敗次數
    pub stuck: i64,      // 卡關診斷已用次數
    pub stuck_at: i64,   // 上次卡關診斷時間（unix 秒）
    pub consulted: bool, // 本 session 是否已用過 consult_advisor（提醒收聲）
}

fn state_path(sess: &str) -> PathBuf {
    state_dir().join(format!("{sess}.state.json"))
}

pub fn state_dir() -> PathBuf {
    data_dir().join("state")
}

fn load_state(sess: &str) -> HookState {
    let st = load_state_at(&state_path(sess));
    // 新鮮度重置只適用於拿不到 session id 的 "default" 案例；
    // 真實 session id 每次都是新的，計數不該跨 session 被誤清。
    if sess == "default" && now_secs().saturating_sub(st.ts) > STATE_STALE_AFTER_SECS {
        logger::info("state stale reset sess=default");
        return HookState::default();
    }
    st
}

fn load_state_at(path: &Path) -> HookState {
    // 對應 Go「json.Unmarshal 錯誤被忽略、保留已解到的欄位」的部分保鮮語義：
    // 走 Value 逐欄盡力萃取，缺欄/null/型別不合回零值。
    // 檔案存在但整體不是 JSON → 全零＋留痕（這會讓提醒重新發聲，值得可追查）
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
            // 寫不進去 → 提醒不會收聲（下次再提醒）；留痕供追查
            logger::debug(&format!("state write failed path={}", path.display()));
        }
    }
}

// sessionKey：env 的 CLAUDE_SESSION_ID 優先，其次 stdin 的 session_id，
// 都沒有就用 "default"（靠 STATE_STALE_AFTER_SECS 避免跨 session 累計）。
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

// markConsulted：MCP 工具被呼叫過即標記，開場提醒就此收聲。
// 諮詢成敗都算數——提醒的目的是確認主模型知道工具存在，呼叫過就達成了。
pub fn mark_consulted(session_id: &str) {
    if session_id.is_empty() {
        return;
    }
    let k = sanitize_session(session_id);
    let mut st = load_state(&k);
    st.consulted = true;
    save_state(&k, &st);
}

// sanitizeSession：保留 [A-Za-z0-9._-]，其餘換 '_'，上限 64 位元組
// （合法字元與替換字皆 1 byte，位元組數＝字元數）。
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

// debugLog：把每次 hook 的原始輸入留檔（「ZCode 餵了什麼」），供確認實際欄位名；
// 超過 2MB 就輪替。行為軌跡在 advisor.log（logger 模組），兩者職責不同。
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
        // 沿用 Go 版 json tags 的欄位名
        for key in [r#""ts":1725500000"#, r#""open":2"#, r#""fail":1"#, r#""stuck":3"#, r#""stuck_at":1725490000"#, r#""consulted":true"#] {
            assert!(raw.contains(key), "missing {key} in {raw}");
        }
        assert_eq!(load_state_at(&path), st);
        // 部分/畸形狀態檔：比照 Go「保留已解到的欄位、其餘回零值」
        fs::write(&path, r#"{"open":2,"fail":-5,"consulted":"yes"}"#).unwrap();
        let st = load_state_at(&path);
        assert_eq!(st.open, 2);
        assert_eq!(st.fail, -5);
        assert_eq!(st.ts, 0);
        assert!(!st.consulted);
        // null 欄位 = no-op（Go 語義），非缺失
        fs::write(&path, r#"{"open":2,"fail":null}"#).unwrap();
        assert_eq!(load_state_at(&path).open, 2);
        assert_eq!(load_state_at(&path).fail, 0);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn sanitize_matches_go_rules() {
        assert_eq!(sanitize_session("sess_abc-123.jsonl"), "sess_abc-123.jsonl");
        assert_eq!(sanitize_session("a/b\\c:d"), "a_b_c_d");
        assert_eq!(sanitize_session("你好"), "__");
        let long = "x".repeat(100);
        assert_eq!(sanitize_session(&long).len(), 64);
    }

    #[test]
    fn failure_threshold_and_cooldown_state_machine() {
        // 直接驗證 hookPostToolUseFailure 的計數邏輯（不打 API 的前置路徑）
        let sess = sanitize_session("smoke-sm-1");
        let path = state_path(&sess);
        let _ = fs::remove_file(&path);

        let mut st = load_state(&sess);
        st.fail += 1;
        assert!(st.fail < STUCK_FAIL_THRESHOLD); // 第一次失敗：未達門檻
        st.fail += 1;
        assert!(st.fail >= STUCK_FAIL_THRESHOLD); // 第二次：達門檻
        st.fail = 0;
        st.stuck += 1;
        st.stuck_at = now_secs();
        save_state(&sess, &st);

        let st2 = load_state(&sess);
        assert_eq!(st2.stuck, 1);
        assert!(now_secs() - st2.stuck_at < STUCK_COOLDOWN_SECS); // 冷卻中
        let _ = fs::remove_file(&path);
    }
}
