// 共用小工具：截斷、家目錄、時間、資料目錄。

use std::path::PathBuf;

// truncate：按位元組數截斷（對應 Go 的 s[:n]）。Go 容忍切在多位元組字元中間
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
// 都沒有時回空路徑，讓上層路徑「壞得無害」。
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

// data_dir：執行期產物（state/、advisor.log、hooks-debug.log）的跨平台根目錄。
// dirs::data_local_dir() 落在各 OS 慣例位置：Linux ~/.local/share（尊重
// $XDG_DATA_HOME）、macOS ~/Library/Application Support、Windows %LOCALAPPDATA%；
// 解析失敗（罕見）退回 home 下的 .zcode-advisor。
// 刻意不用 Go 版的 ~/.zcode/zcode-advisor——那是 Go 遺留目錄，Rust 版與之脫鉤。
pub fn data_dir() -> PathBuf {
    match dirs::data_local_dir() {
        Some(d) => d.join("zcode-advisor"),
        None => home_dir().join(".zcode-advisor"),
    }
}

// unix 秒。時鐘不可用時回 0（與 Go 版 time.Now 失敗的後果同級：狀態計數失效而已）。
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// rfc3339_utc：不引入 chrono 的最小實作（Howard Hinnant 的 civil_from_days 演算法）。
// UTC 時間戳（日誌與人類可讀輸出用）；本地時區不值得為此拉 tz 資料庫。
pub fn rfc3339_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        m,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// 建立私有目錄（unix: 0700；Windows 走 profile 預設 ACL）。已存在視為成功。
pub fn create_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).recursive(true).create(dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

// 以私有權限開檔（unix: 0600；Windows 同上走預設 ACL）。
pub fn open_private_append(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::OpenOptions::new().create(true).append(true).open(path)
    }
}

pub fn open_private_write(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(path)
    }
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

    #[test]
    fn rfc3339_known_values() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(1_725_500_000), "2024-09-05T01:33:20Z"); // 與 date -u 對拍
        assert_eq!(rfc3339_utc(951_782_400), "2000-02-29T00:00:00Z"); // 閏年
    }

    #[test]
    fn data_dir_is_namespaced() {
        // 不論落在哪，最後一段必須是 zcode-advisor（跨平台一致性由 dirs 保證）
        let d = data_dir();
        assert!(d.ends_with("zcode-advisor"), "{d:?}");
    }
}
