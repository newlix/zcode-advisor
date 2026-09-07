//! zcode-advisor: a Rust reimplementation of the retired Go version
//! (~/.zcode/zcode-advisor). One binary, two modes (MCP stdio server / hook),
//! three trigger points; design details in the README.
//! The MCP protocol layer uses the official rmcp; the advisor backend is
//! selectable in an optional TOML config file (config.rs): local Ollama via a
//! hand-written plain-HTTP client (default), the Claude Code CLI via a
//! subprocess (claude.rs), or any OpenAI-compatible HTTPS endpoint via ureq.
//! Hook output is hand-written.
//! Runtime artifacts (state, advisor.log, hooks-debug.log) live in the
//! OS-conventional data directory (util::data_dir).

pub mod claude;
pub mod config;
pub mod hooks;
pub mod http;
pub mod logger;
pub mod rollout;
pub mod server;
pub mod util;
