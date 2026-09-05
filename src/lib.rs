//! zcode-advisor：Go 版（~/.zcode/zcode-advisor，已退役）功能的 Rust 重製。
//! 一個 binary、兩種模式（MCP stdio server / hook）、三個觸發點，設計細節見 README。
//! MCP 協議層用官方 rmcp；對 Ollama 的單發純 HTTP 與 hook 輸出為手寫實作。
//! 執行期產物（state、advisor.log、hooks-debug.log）落在各 OS 慣例的資料目錄（util::data_dir）。

pub mod hooks;
pub mod http;
pub mod logger;
pub mod rollout;
pub mod server;
pub mod util;
