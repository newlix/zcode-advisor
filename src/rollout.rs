// Rollout lookup: identify "which session is calling consult_advisor".
// The response that calls the tool is necessarily already on disk (tool
// execution happens after the response completes), so among recently active
// non-subagent rollout files, find the one whose last line carries a
// consult_advisor toolCall with an exactly matching question, and use its
// sessionId for attribution — no mtime guessing, no env-var dependence, and
// correct attribution even with parallel ZCode sessions.
// This file corresponds to the Go version's rollout.go; parsing goes through
// serde_json::Value (matching Go's two-stage attempt).

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::util::truncate;

const ROLLOUT_TAIL: usize = 48_000; // conversation intake cap (chars); 0 = disabled
const ACTIVE_WINDOW_SECS: u64 = 10 * 60; // only look at files active in the last 10 minutes
const MAX_CANDIDATES: usize = 10; // defensive cap: check only the 10 most recently active files

pub struct RolloutMatch {
    pub session_id: String,
    pub dialog: String,   // condensed, tail-truncated conversation text (role: content)
    pub preamble: String, // the executor's current-turn monologue before calling the tool (response.text)
    pub path: PathBuf,
}

// Tests that want a different directory pass it into find_calling_session_in
// (the Go version achieved the same via a package var).
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
        // compare in unix seconds so SystemTime subtraction can't break on clock
        // rollback; a future mtime (> now) also counts as active
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
    files.sort_by(|a, b| b.1.cmp(&a.1)); // by mtime, newest → oldest

    'files: for f in files.iter().take(MAX_CANDIDATES) {
        let Some(line) = last_complete_line(&f.0) else { continue };
        let Ok(rec) = serde_json::from_str::<Value>(&line) else { continue };

        // Type checks mirror the Go side's typed-struct deserialization: a
        // wrong (non-null) type → skip the whole file; JSON null is a no-op
        // (treated as missing) — the key difference between encoding/json and
        // serde.
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
                // Element shapes must also pass Go's typed decode: a non-object
                // element, non-string name, or non-object input makes Go skip
                // the whole file (even if a matching toolCall appears later)
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

// last_complete_line reads at most 16MB from the file tail and scans backwards
// for the first complete parseable line; a segment truncated at the window
// start fails JSON validation and is skipped naturally.
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

// condense_messages flattens request.messages into plain conversation text;
// skips system (large, little value to the advisor), keeps only names and
// truncated arguments for tool_use, and finally keeps the tail (newest
// content) within cap. The Go version truncates by bytes and tolerates half a
// character; Rust here falls back to the char boundary (see util::truncate).
pub fn condense_messages(messages: Option<&Value>, cap: usize) -> String {
    let raw = match messages {
        Some(v) if !v.is_null() => v,
        _ => return String::new(),
    };
    let arr = match raw.as_array() {
        Some(a) => a,
        None => return String::new(), // Go: unmarshal failure returns ""
    };
    let mut b = String::new();
    for m in arr {
        if !m.is_object() {
            return String::new(); // Go: typed-struct deserialization fails for the whole batch → ""
        }
        if let Some(r) = m.get("role") {
            if !r.is_string() && !r.is_null() {
                return String::new(); // same as above; role:null is a no-op in Go → ""
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
        // align to the start of a line to avoid a half-line head (keeps the Go
        // behavior of "cut to after the first newline", quirks included)
        match s.find('\n') {
            Some(i) => s[i + 1..].to_string(),
            None => s.to_string(),
        }
    } else {
        b
    }
}

// contentText: a string content is used directly; a blocks array yields text
// or tool_use summaries. Matches the Go two-stage attempt (string first, then
// []map[string]any).
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
            None => return String::new(), // Go: []map[string]any deserialization failure returns ""
        };
        let text = obj.get("text").and_then(Value::as_str).unwrap_or("");
        if !text.trim().is_empty() {
            b.push_str(text);
            b.push('\n');
            continue;
        }
        let name = obj.get("name").and_then(Value::as_str).unwrap_or("");
        if !name.is_empty() {
            // with the input field missing, Go's json.Marshal(nil) prints "null";
            // serde prints "null" for Value::Null the same way
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

    // Fixture contents are intentionally CJK: the rollout path must handle
    // multibyte text (trim, condense, byte-vs-char boundaries) exactly as real
    // sessions produce it.
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
        // a subagent file must be skipped even when it matches
        write_rollout(&dir, "model-io-sess_subagent-1.jsonl", &[sample_rollout("Q2")]);

        let m = find_calling_session_in(&dir, "Q2").expect("should match");
        assert_eq!(m.session_id, "sess_abc12345-1111");
        assert_eq!(m.preamble, "我想先諮詢顧問再動手"); // trimmed
        assert!(m.dialog.contains("user: 請幫我修這個 bug"));
        assert!(m.dialog.contains("user: 繼續"));
        assert!(m.dialog.contains("assistant: 我先看看檔案\n"));
        assert!(m.dialog.contains("[tool_use Read "));
        assert!(!m.dialog.contains("SYSTEM PROMPT"));
        // tool_use input truncated to 200 bytes
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
        // tail line truncated mid-way (no newline, incomplete JSON) → unusable
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("model-io-sess_t1.jsonl"), &bytes[..bytes.len() - 10]).unwrap();

        let m = find_calling_session_in(&dir, "Q3");
        assert!(m.is_none(), "the file's only line is incomplete and must not be used");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn condense_tail_cap_aligns_to_char_boundary_and_line() {
        // Three messages of all-3-byte CJK ("我很好" = 9 bytes × 2222 = 19998
        // bytes per message); cap=48000 → the cut lands inside the first
        // message (and (12015-6) % 3 == 0, exactly on a char boundary, matching
        // the Go byte-exact cut); after line alignment only the complete second
        // and third lines remain. (Intentionally CJK to exercise multibyte
        // boundaries.)
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
        // the trailing double newline matches the Go behavior: each block
        // already carries \n, plus one more at line end
        assert_eq!(
            s,
            "user: plain string\nassistant: block text\n[tool_use Bash {\"command\":\"ls\"}]\n[tool_use NoInput null]\n\n"
        );
    }
}
