// rollout 反查：辨識「是哪個 session 在呼叫 consult_advisor」。
// 呼叫工具的那個 response 必然已落盤（工具執行晚於 response 完成），
// 因此在最近活躍的非 subagent rollout 檔中，找「最後一行帶有 question 完全一致的
// consult_advisor toolCall」的檔，以其 sessionId 做辨識——不猜 mtime、不依賴環境變數，
// 多個 ZCode session 並行時也能正確歸屬。
// 本檔對應 Go 版的 rollout.go；解析走 serde_json::Value（對應 Go 的兩段式嘗試）。

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::util::truncate;

const ROLLOUT_TAIL: usize = 48_000; // 對話帶入上限（字元）；0 = 關閉
const ACTIVE_WINDOW_SECS: u64 = 10 * 60; // 只看最近 10 分鐘活躍的檔
const MAX_CANDIDATES: usize = 10; // 防禦性上限：只查最近活躍的 10 個檔

pub struct RolloutMatch {
    pub session_id: String,
    pub dialog: String,   // 壓縮、截尾後的對話文字（role: content）
    pub preamble: String, // executor 呼叫工具前的當前輪獨白（response.text）
    pub path: PathBuf,
}

// 測試想換目錄就傳入 find_calling_session_in（Go 版以 package var 達成同一目的）。
pub fn rollout_dir() -> PathBuf {
    crate::util::home_dir().join(".zcode").join("cli").join("rollout")
}

pub fn find_calling_session(question: &str) -> Option<RolloutMatch> {
    find_calling_session_in(&rollout_dir(), question)
}

pub fn find_calling_session_in(dir: &Path, question: &str) -> Option<RolloutMatch> {
    if ROLLOUT_TAIL == 0 {
        return None;
    }
    let entries = fs::read_dir(dir).ok()?;
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let cutoff = now_secs.saturating_sub(ACTIVE_WINDOW_SECS);
    let mut files: Vec<(PathBuf, u64)> = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("model-io-sess_") || !name.ends_with(".jsonl") || name.contains("subagent") {
            continue;
        }
        let Ok(md) = e.metadata() else { continue };
        // unix 秒比較避免 SystemTime 相減在時鐘回撥時出錯；未來 mtime（> now）也算活躍
        let modified = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if modified > cutoff {
            files.push((e.path(), modified));
        }
    }
    files.sort_by(|a, b| b.1.cmp(&a.1)); // mtime 新→舊

    'files: for f in files.iter().take(MAX_CANDIDATES) {
        let Some(line) = last_complete_line(&f.0) else { continue };
        let Ok(rec) = serde_json::from_str::<Value>(&line) else { continue };

        // 型別檢查比照 Go 端 typed struct 的反序列化：型別不合（非 null）→ 整檔跳過；
        // JSON null 是 no-op（視同缺欄），這是 encoding/json 與 serde 的關鍵差異。
        if let Some(v) = rec.get("sessionId") {
            if !v.is_string() && !v.is_null() {
                continue;
            }
        }
        if let Some(r) = rec.get("request") {
            if !r.is_object() && !r.is_null() {
                continue;
            }
        }
        let response = rec.get("response");
        if let Some(r) = response {
            if !r.is_object() && !r.is_null() {
                continue;
            }
            if let Some(t) = r.get("text") {
                if !t.is_string() && !t.is_null() {
                    continue;
                }
            }
            if let Some(tc) = r.get("toolCalls") {
                if !tc.is_array() && !tc.is_null() {
                    continue;
                }
                // 元素形狀同樣要過 Go 的 typed decode：非物件元素、name 非字串、
                // input 非物件，任何一項都會讓 Go 整檔跳過（即使後面有匹配的 toolCall）
                for c in tc.as_array().into_iter().flatten() {
                    if !c.is_object() && !c.is_null() {
                        continue 'files;
                    }
                    if let Some(n) = c.get("name") {
                        if !n.is_string() && !n.is_null() {
                            continue 'files;
                        }
                    }
                    if let Some(i) = c.get("input") {
                        if !i.is_object() && !i.is_null() {
                            continue 'files;
                        }
                    }
                }
            }
        }

        let matched = response
            .and_then(|r| r.get("toolCalls"))
            .and_then(Value::as_array)
            .map(|cs| {
                cs.iter().any(|c| {
                    let name_ok = c.get("name").and_then(Value::as_str).is_some_and(|n| n.contains("consult_advisor"));
                    name_ok && c.get("input").and_then(|i| i.get("question")).and_then(Value::as_str) == Some(question)
                })
            })
            .unwrap_or(false);
        if !matched {
            continue;
        }
        return Some(RolloutMatch {
            session_id: rec.get("sessionId").and_then(Value::as_str).unwrap_or("").to_string(),
            dialog: condense_messages(rec.get("request").and_then(|r| r.get("messages")), ROLLOUT_TAIL),
            preamble: truncate(
                response.and_then(|r| r.get("text")).and_then(Value::as_str).unwrap_or("").trim(),
                4000,
            ),
            path: f.0.clone(),
        });
    }
    None
}

// last_complete_line 讀檔尾最多 16MB，從尾端往回找第一個完整可解析的行；
// 視窗開頭被截斷的段落會因 JSON 驗證失敗被自然跳過。
fn last_complete_line(path: &Path) -> Option<String> {
    let md = fs::metadata(path).ok()?;
    let size = md.len();
    if size == 0 {
        return None;
    }
    let mut f = fs::File::open(path).ok()?;
    let w = size.min(16 << 20);
    f.seek(SeekFrom::Start(size - w)).ok()?;
    let mut buf = vec![0u8; w as usize];
    let mut filled = 0;
    loop {
        if filled == buf.len() {
            break;
        }
        match f.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
    }
    buf.truncate(filled);
    for seg in buf.split(|&b| b == b'\n').rev() {
        let s = String::from_utf8_lossy(seg).trim().to_string();
        if s.is_empty() || serde_json::from_str::<Value>(&s).is_err() {
            continue;
        }
        return Some(s);
    }
    None
}

// condense_messages 把 request.messages 壓成純文字對話；跳過 system（體積大、對顧問
// 幫助小），tool_use 只留名稱與截斷的參數，最後保留尾端（最新內容）不超過 cap。
// Go 版按位元組截尾且容忍半個字元；Rust 這裡退到 char boundary（見 util::truncate）。
pub fn condense_messages(messages: Option<&Value>, cap: usize) -> String {
    let raw = match messages {
        Some(v) if !v.is_null() => v,
        _ => return String::new(),
    };
    let arr = match raw.as_array() {
        Some(a) => a,
        None => return String::new(), // Go：unmarshal 失敗回 ""
    };
    let mut b = String::new();
    for m in arr {
        if !m.is_object() {
            return String::new(); // Go：typed struct 反序列化整批失敗回 ""
        }
        if let Some(r) = m.get("role") {
            if !r.is_string() && !r.is_null() {
                return String::new(); // 同上；role:null 在 Go 是 no-op → ""
            }
        }
        let role = m.get("role").and_then(Value::as_str).unwrap_or("");
        if role == "system" {
            continue;
        }
        let text = content_text(m.get("content"));
        if text.trim().is_empty() {
            continue;
        }
        b.push_str(&format!("{role}: {text}\n"));
    }
    if b.len() > cap {
        let start = b.floor_char_boundary(b.len() - cap);
        let s = &b[start..];
        // 對齊行首，避免半行開頭（沿用 Go 版「切到第一個換行之後」的行為，含其 quirk）
        match s.find('\n') {
            Some(i) => s[i + 1..].to_string(),
            None => s.to_string(),
        }
    } else {
        b
    }
}

// contentText：content 是字串就直接用；是 blocks 陣列則取 text 或 tool_use 摘要。
// 對應 Go 版的兩段式嘗試（先 string 再 []map[string]any）。
fn content_text(content: Option<&Value>) -> String {
    let raw = match content {
        Some(v) if !v.is_null() => v,
        _ => return String::new(),
    };
    if let Some(s) = raw.as_str() {
        return s.to_string();
    }
    let blocks = match raw.as_array() {
        Some(a) => a,
        None => return String::new(),
    };
    let mut b = String::new();
    for blk in blocks {
        let obj = match blk.as_object() {
            Some(o) => o,
            None => return String::new(), // Go：[]map[string]any 反序列化失敗回 ""
        };
        let text = obj.get("text").and_then(Value::as_str).unwrap_or("");
        if !text.trim().is_empty() {
            b.push_str(text);
            b.push('\n');
            continue;
        }
        let name = obj.get("name").and_then(Value::as_str).unwrap_or("");
        if !name.is_empty() {
            // 缺 input 欄位時 Go json.Marshal(nil) 輸出 "null"，serde 對 Value::Null 同樣輸出 "null"
            let input = obj.get("input").unwrap_or(&Value::Null);
            let input_json = serde_json::to_string(input).unwrap_or_else(|_| "null".into());
            b.push_str(&format!("[tool_use {name} {}]\n", truncate(&input_json, 200)));
        }
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write_rollout(dir: &Path, name: &str, lines: &[String]) {
        fs::create_dir_all(dir).unwrap();
        let mut content = lines.join("\n");
        content.push('\n');
        fs::write(dir.join(name), content).unwrap();
    }

    fn sample_rollout(question: &str) -> String {
        json!({
            "sessionId": "sess_abc12345-1111",
            "request": {"messages": [
                {"role": "system", "content": "SYSTEM PROMPT 應被略過"},
                {"role": "user", "content": "請幫我修這個 bug"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "我先看看檔案"},
                    {"type": "tool_use", "name": "Read", "input": {"file_path": "/tmp/x.go", "pad": "填充".repeat(120)}}
                ]},
                {"role": "user", "content": "繼續"}
            ]},
            "response": {
                "text": "  我想先諮詢顧問再動手  ",
                "toolCalls": [{"name": "mcp__zcode-advisor__consult_advisor", "input": {"question": question, "context": "x"}}]
            }
        })
        .to_string()
    }

    #[test]
    fn finds_matching_session_and_condenses() {
        let dir = std::env::temp_dir().join(format!("zca-rollout-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        write_rollout(&dir, "model-io-sess_abc12345-1111.jsonl", &[sample_rollout("Q1"), sample_rollout("Q2")]);
        // subagent 檔即使匹配也必須被略過
        write_rollout(&dir, "model-io-sess_subagent-1.jsonl", &[sample_rollout("Q2")]);

        let m = find_calling_session_in(&dir, "Q2").expect("should match");
        assert_eq!(m.session_id, "sess_abc12345-1111");
        assert_eq!(m.preamble, "我想先諮詢顧問再動手"); // trim 過
        assert!(m.dialog.contains("user: 請幫我修這個 bug"));
        assert!(m.dialog.contains("user: 繼續"));
        assert!(m.dialog.contains("assistant: 我先看看檔案\n"));
        assert!(m.dialog.contains("[tool_use Read "));
        assert!(!m.dialog.contains("SYSTEM PROMPT"));
        // tool_use input 截 200 位元組
        let tu = m.dialog.lines().find(|l| l.starts_with("[tool_use")).unwrap();
        assert!(tu.contains("…(truncated)"), "{tu}");

        assert!(find_calling_session_in(&dir, "no-such-question").is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn skips_incomplete_tail_line() {
        let dir = std::env::temp_dir().join(format!("zca-rollout-tail-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let bytes = sample_rollout("Q3").into_bytes();
        // 尾行從中間截斷（無換行、JSON 不完整）→ 不可採用
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("model-io-sess_t1.jsonl"), &bytes[..bytes.len() - 10]).unwrap();

        let m = find_calling_session_in(&dir, "Q3");
        assert!(m.is_none(), "檔裡唯一的行不完整，不該被採用");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn condense_tail_cap_aligns_to_char_boundary_and_line() {
        // 三則全 3-byte CJK 的訊息（「我很好」=9 bytes × 2222 = 19998 bytes/則），
        // cap=48000 → 截點落在第一則訊息中間（且 (12015-6) % 3 == 0，正好在 char
        // boundary 上，與 Go 版逐位元組切一致）；對齊行首後應只剩第二、三則完整行
        let m1 = "你好嗎".repeat(6666);
        let m2 = "我很好".repeat(2222);
        let m3 = "再見啦".repeat(2222);
        let line2 = format!("user: {m2}\n");
        let line3 = format!("user: {m3}\n");
        let msgs = json!([
            {"role": "user", "content": m1},
            {"role": "user", "content": m2},
            {"role": "user", "content": m3},
        ]);
        let s = condense_messages(Some(&msgs), 48000);
        assert_eq!(s, format!("{line2}{line3}"));
    }

    #[test]
    fn condense_handles_string_and_block_contents() {
        let msgs = json!([
            {"role": "system", "content": "skip"},
            {"role": "user", "content": "plain string"},
            {"role": "assistant", "content": [
                {"type": "text", "text": "block text"},
                {"type": "text", "text": "   "},
                {"type": "tool_use", "name": "Bash", "input": {"command": "ls"}},
                {"type": "tool_use", "name": "NoInput"}
            ]},
            {"role": "user", "content": 42}
        ]);
        let s = condense_messages(Some(&msgs), 1000);
        // 結尾雙換行是 Go 版同款行為：每個 block 已帶 \n，行尾再加一個
        assert_eq!(
            s,
            "user: plain string\nassistant: block text\n[tool_use Bash {\"command\":\"ls\"}]\n[tool_use NoInput null]\n\n"
        );
    }
}
