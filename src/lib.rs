//! zcode-advisor：Go 版（~/.zcode/zcode-advisor）的 Rust 移植。
//! 一個 binary、兩種模式（MCP stdio server / hook）、三個觸發點，設計細節見 README。
//! serde_json 是唯一的解析依賴；HTTP 走手寫的 localhost 純 HTTP client（http 模組），
//! 延續 Go 版「單一 binary、零重依賴」的性質。

pub mod hooks;
pub mod http;
pub mod rollout;
pub mod server;
pub mod util;
