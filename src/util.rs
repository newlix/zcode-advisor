// Shared utilities: truncation, home directory, time, data directory.

use std::path::PathBuf;

// truncate: cut by byte count (matching Go's s[:n]). Go tolerates cutting
// inside a multibyte character (producing invalid UTF-8); a Rust slice must
// land on a char boundary or it panics — the most explosive difference when
// porting. We uniformly align with floor_char_boundary to "the largest legal
// boundary not exceeding n bytes", at the cost of cutting up to 2 bytes less
// than Go.
pub fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let end = s.floor_char_boundary(n);
    format!("{}…(truncated)", &s[..end])
}

// std::env::home_dir is deprecated; hand-written HOME → USERPROFILE fallback
// (Unix → Windows). Neither present → empty path, keeping downstream paths
// harmlessly broken.
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

// data_dir: the cross-platform root for runtime artifacts (state/,
// advisor.log, hooks-debug.log). dirs::data_local_dir() lands in each OS's
// conventional location: Linux ~/.local/share (honoring $XDG_DATA_HOME),
// macOS ~/Library/Application Support, Windows %LOCALAPPDATA%; on resolution
// failure (rare) it falls back to .zcode-advisor under home. Deliberately not
// the Go version's ~/.zcode/zcode-advisor — that's the legacy Go directory;
// this version is decoupled from it.
pub fn data_dir() -> PathBuf {
    match dirs::data_local_dir() {
        Some(d) => d.join("zcode-advisor"),
        None => home_dir().join(".zcode-advisor"),
    }
}

// unix seconds. Falls back to 0 when the clock is unavailable (same
// consequence as the Go version's time.Now failing: the state counters just
// stop working).
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// rfc3339_utc: a minimal implementation without pulling chrono (Howard
// Hinnant's civil_from_days algorithm). UTC timestamps (for logs and
// human-readable output); local time isn't worth a tz database.
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

// Create a private directory (unix: 0700; Windows relies on the profile's
// default ACL). Already existing counts as success.
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

// Open a file with private permissions (unix: 0600; Windows likewise via the
// default ACL).
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
        // 3-byte CJK: n landing mid-character falls back to the boundary
        // (Go would cut out half a character). Intentionally multibyte.
        assert_eq!(truncate("你好世界", 4), "你…(truncated)");
        assert_eq!(truncate("你好世界", 6), "你好…(truncated)");
        // 4-byte emoji
        assert_eq!(truncate("a😀b😀c", 3), "a…(truncated)");
    }

    #[test]
    fn rfc3339_known_values() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(1_725_500_000), "2024-09-05T01:33:20Z"); // cross-checked against date -u
        assert_eq!(rfc3339_utc(951_782_400), "2000-02-29T00:00:00Z"); // leap year
    }

    #[test]
    fn data_dir_is_namespaced() {
        // wherever it lands, the last component must be zcode-advisor
        // (cross-platform consistency guaranteed by dirs)
        let d = data_dir();
        assert!(d.ends_with("zcode-advisor"), "{d:?}");
    }
}
