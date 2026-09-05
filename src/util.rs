// 共用小工具。對應 Go 版散在 hooks.go 裡的 truncate 與 os.UserHomeDir。

use std::path::PathBuf;

// truncate：對應 Go 的 s[:n]（按位元組數）。Go 容忍切在多位元組字元中間
// （產生無效 UTF-8），Rust 切片必須落在 char boundary 上否則 panic——
// 這是移植時最容易炸的差異點，統一用 floor_char_boundary 對齊到
// 「不超過 n 位元組的最大合法邊界」，代價是比 Go 少切最多 2 個位元組。
pub fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let end = s.floor_char_boundary(n);
    format!("{}…(truncated)", &s[..end])
}

// std::env::home_dir 已棄用；這裡手寫 HOME → USERPROFILE fallback（Unix → Windows）。
// 都沒有時回空路徑，與 Go 版拿到錯誤後硬拼出來的相對路徑一樣只是「壞得無害」。
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

// unix 秒。時鐘不可用時回 0（與 Go 版 time.Now 失敗的後果同級：狀態計數失效而已）。
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_ascii_and_boundaries() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdefghij", 10), "abcdefghij");
        assert_eq!(truncate("abcdefghijk", 10), "abcdefghij…(truncated)");
        // 3 位元組 CJK：n 落在字元中間時退到邊界（Go 會切出半個字）
        assert_eq!(truncate("你好世界", 4), "你…(truncated)");
        assert_eq!(truncate("你好世界", 6), "你好…(truncated)");
        // 4 位元組 emoji
        assert_eq!(truncate("a😀b😀c", 3), "a…(truncated)");
    }
}
