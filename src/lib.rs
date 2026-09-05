//! zcode-advisor: a Rust reimplementation of the retired Go version
//! (~/.zcode/zcode-advisor). One binary, two modes (MCP stdio server / hook),
//! three trigger points; design details in the README.
//! The MCP protocol layer uses the official rmcp; the one-shot plain-HTTP call
//! to Ollama and the hook output are hand-written.
//! Runtime artifacts (state, advisor.log, hooks-debug.log) live in the
//! OS-conventional data directory (util::data_dir).

pub mod hooks;
pub mod http;
pub mod logger;
pub mod rollout;
pub mod server;
pub mod util;
