// 行為軌跡日誌（advisor.log）：與 hooks-debug.log 分工——那邊記「ZCode 餵了什麼」
// （原始 stdin 擷取），這邊記「我們做了什麼、為什麼」（consult 生命週期、
// hook 決策點）。目的是事後追查：顧問為什麼這樣回答、hook 為什麼觸發／沒觸發。
//
// 只有結構性軌跡（決策、結果、時間、大小），不記內容——「顧問到底看到什麼」
// （完整 question、對話、建議本文）由 ZCode 的 rollout 檔原生保存
// （~/.zcode/cli/rollout/），事後永遠可重現，日誌不重複捕。
//
// 設計約束：
// - 記錄系統絕不能讓任務失敗：任何寫檔錯誤靜默丟棄（同 hooks-debug 哲學）。
// - 多 process 並發（兩條 MCP 連線＋一次性的 hook process）：O_APPEND＋
//   單一 write_all（行長遠小於 page size）保持行完整；每行開檔即關，
//   頻率極低（每次 consult／hook 一兩行），無需持有檔案。
// - stdout 永遠不留痕跡（MCP 協議通道）；stderr 的既有行照舊，這裡只寫檔案。
// - 無等級開關、無環境變數：兩個等級（INFO/ERROR）都是severity標記，
//   全時全開——行為軌跡的量本來就該全記，內容另有歸宿。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::util::{create_private_dir, now_secs, open_private_append, rfc3339_utc};

const ROTATE_BYTES: u64 = 2 << 20; // 與 hooks-debug.log 同門檻

// 等級僅是 severity 標記（供 grep 過濾 ERROR），不過濾輸出。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Error,
    Info,
}

impl Level {
    fn name(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Info => "INFO",
        }
    }
}

// 模式（server|hook）由 process 啟動時指定，落在每行前綴供跨 process 對齊。
static MODE: OnceLock<&'static str> = OnceLock::new();

// init 標記本 process 的模式（server 或 hook）；未呼叫時 log() 仍可用，mode 記為 "?"。
pub fn init(mode: &'static str) {
    let _ = MODE.set(mode);
}

pub fn info(msg: &str) {
    log(Level::Info, msg);
}

pub fn error(msg: &str) {
    log(Level::Error, msg);
}

fn log(level: Level, msg: &str) {
    let mode = *MODE.get().unwrap_or(&"?");
    let line = format!(
        "{} pid={} mode={} {} {}\n",
        rfc3339_utc(now_secs()),
        std::process::id(),
        mode,
        level.name(),
        msg
    );
    // 失敗哲學：寫不進去就丟棄——日誌缺席不可耽誤正事
    let _ = write_line(&log_path(), line.as_bytes());
}

fn log_path() -> PathBuf {
    crate::util::data_dir().join("advisor.log")
}

// write_line：超過輪替門檻先 rotate，再以 O_APPEND 單次寫入整行。
fn write_line(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Ok(md) = std::fs::metadata(path) {
        if md.len() > ROTATE_BYTES {
            rotate(path);
        }
    }
    if let Some(dir) = path.parent() {
        create_private_dir(dir)?;
    }
    let mut f = open_private_append(path)?;
    f.write_all(bytes)
}

// rotate：改名保留一代（advisor.log.1）；改名失敗（例如被佔用）以 truncate 重開兜底。
// 已知接受的 race：多 process 同時跨過輪替門檻時，後到者的 rename 可能覆掉剛搬去的
// 世代（損失舊 .1，不損當前檔與行完整性）——發生窗口是 ns～µs 級且需兩 process
// 同瞬間輪替，以「盡力而為」語義接受（靜默失敗哲學的延伸）。
fn rotate(path: &Path) {
    let mut prev = path.as_os_str().to_os_string();
    prev.push(".1");
    if std::fs::rename(path, Path::new(&prev)).is_err() {
        let _ = crate::util::open_private_write(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_log(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("zca-log-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        p
    }

    fn log_to(path: &Path, msg: &str) {
        write_line(path, format!("{}\n", msg).as_bytes()).unwrap();
    }

    #[test]
    fn appends_lines_to_same_file() {
        let dir = temp_log("append");
        let p = dir.join("advisor.log");
        log_to(&p, "line1");
        log_to(&p, "line2");
        let content = fs::read_to_string(&p).unwrap();
        assert_eq!(content, "line1\nline2\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotates_over_threshold_keeping_one_generation() {
        let dir = temp_log("rotate");
        let p = dir.join("advisor.log");
        log_to(&p, "first-line");
        // 以 append 把檔案撐過輪替門檻（模擬長期累積；fs::write 會截斷不能用）
        let junk = vec![b'x'; (ROTATE_BYTES + 1) as usize];
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            use std::io::Write as _;
            let mut f = fs::OpenOptions::new().append(true).mode(0o600).open(&p).unwrap();
            f.write_all(&junk).unwrap();
        }
        log_to(&p, "after-rotate");
        // 整個舊檔（first-line + junk）搬到 .1；新檔只有 after-rotate
        let rotated = fs::read(dir.join("advisor.log.1")).unwrap();
        assert!(rotated.starts_with(b"first-line\n"), "舊內容應保留在 .1");
        assert!(rotated.len() > ROTATE_BYTES as usize);
        assert_eq!(fs::read_to_string(&p).unwrap(), "after-rotate\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_writers_do_not_corrupt_lines() {
        let dir = temp_log("conc");
        let p = dir.join("advisor.log");
        let writers: Vec<_> = (0..4)
            .map(|w| {
                let p = p.clone();
                std::thread::spawn(move || {
                    for i in 0..50 {
                        write_line(&p, format!("w{w}-i{i:03}\n").as_bytes()).unwrap();
                    }
                })
            })
            .collect();
        for h in writers {
            h.join().unwrap();
        }
        let content = fs::read_to_string(&p).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 200, "行數必須完整（無交錯損毀）");
        let mut sorted = lines.clone();
        sorted.sort_unstable();
        let mut expect: Vec<String> = (0..4)
            .flat_map(|w| (0..50).map(move |i| format!("w{w}-i{i:03}")))
            .collect();
        expect.sort_unstable();
        assert_eq!(sorted, expect.iter().map(|s| s.as_str()).collect::<Vec<_>>());
        let _ = fs::remove_dir_all(&dir);
    }
}
