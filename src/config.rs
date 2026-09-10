// Configuration: an optional TOML file selecting the advisor backend and its
// knobs. Absent file → pure defaults: the Claude Code CLI asking fable with
// an opus quota fallback — the zero-configuration path. A file that is
// present but broken (parse error, unknown key, unset ${VAR}, missing
// required field) falls back to defaults too — the advisor must never hold up
// real work — but loudly: the warning travels to stderr, the ERROR log, and
// the prefix of the next consult's result (a silent fallback to claude
// defaults when the user configured openai would waste hours).
//
// Every string value in the file supports environment-variable interpolation:
//   ${VAR}        → value of VAR; unset or empty → config error
//   ${VAR:-fb}    → value of VAR; unset OR empty → the literal fallback "fb"
//   $$            → literal "$"
// Interpolation is single-pass (a fallback containing "${" is never
// re-expanded) and is how secrets (api_key) get in without ever touching
// disk or logs.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use serde::Deserialize;

use crate::util;

// Path override for tests and unusual setups; takes precedence over the
// standard location. (No ZCODE_CONSULTANT_* variable exists in ZCode itself.)
pub const ENV_CONFIG_PATH: &str = "ZCODE_CONSULTANT_CONFIG";

// Built-in defaults — the previously hard-coded values. One knob per concern;
// anything here can be overridden in the config file.
pub const OLLAMA_URL: &str = "http://localhost:11434/v1/chat/completions";
pub const OLLAMA_MODEL: &str = "kimi-k3:cloud";
pub const OLLAMA_MAX_TOKENS: u64 = 131_072; // reasoning models burn max_tokens on thinking; keep generous
pub const CLAUDE_BIN: &str = "claude";
// Default model chain: the CLI's fable alias with an opus model fallback —
// chosen defaults, not "whatever the CLI is set to": the advisor's quality
// bar shouldn't drift with the CLI's own default-model setting. Setting
// model = "" in the config file still means "the CLI's default" (then make
// sure the CLI's default isn't the fallback model — the CLI rejects that).
pub const CLAUDE_MODEL: &str = "fable";
// model fallback: passed to the CLI as its native --fallback-model (comma-
// separated list allowed; the CLI switches models when the primary is
// overloaded or not available). Empty = off.
pub const CLAUDE_FALLBACK_MODEL: &str = "opus";
pub const OPENAI_MAX_TOKENS: u64 = 8_192; // safe floor; bump to 16–32k for thinking models
pub const OLLAMA_TIMEOUT: Duration = Duration::from_secs(90);
// CLI cold start + a full conversation tail can push a one-shot claude call
// past 3 minutes; the advisor budget matches the reviewer's.
pub const CLAUDE_TIMEOUT: Duration = Duration::from_secs(600);
pub const OPENAI_TIMEOUT: Duration = Duration::from_secs(90);
// The reviewer is always a claude CLI agentic call (the tool loop is
// claude-CLI-specific), independent of the advisor backend choice.
pub const REVIEWER_TOOLS: &str = "Read,Grep,Glob"; // whitelist-validated, keep read-only
pub const REVIEWER_TIMEOUT: Duration = Duration::from_secs(600); // agentic reviews are slower

#[derive(Debug, Clone)]
pub enum Backend {
    /// Local Ollama's OpenAI-compatible endpoint (plain HTTP, no auth).
    Ollama { url: String, model: String, max_tokens: u64 },
    /// Claude Code CLI headless mode (`claude -p`), auth via the CLI's login.
    /// `fallback_model` rides the CLI's native `--fallback-model` flag (the
    /// CLI falls back when the primary is overloaded/unavailable).
    Claude { bin: String, model: String, fallback_model: String },
    /// Any OpenAI-compatible chat-completions endpoint over HTTP(S) with a
    /// bearer key. The wire field is `max_tokens` (not the newer
    /// `max_completion_tokens`) — same scope as the Ollama path.
    OpenAi { url: String, model: String, api_key: String, max_tokens: u64 },
}

/// Reviewer knobs (the `review_change` tool). `bin`/`model` default from the
/// `[claude]` section so both tools share one CLI installation by default.
#[derive(Debug, Clone)]
pub struct Reviewer {
    pub bin: String,
    pub model: String, // empty = CLI default
    pub fallback_model: String, // empty = off; inherits [claude].fallback_model
    pub tools: String, // comma-separated, whitelist-validated at load
    pub add_dirs: Vec<String>,
    pub timeout: Duration,
}

impl Reviewer {
    /// Redacted one-line identity for logs.
    pub fn summary(&self) -> String {
        let model = if self.model.trim().is_empty() { "<cli-default>" } else { &self.model };
        let dirs = if self.add_dirs.is_empty() {
            "none".to_string()
        } else {
            self.add_dirs.join(",")
        };
        match self.fallback_model.trim().is_empty() {
            true => format!("model={model} tools={} add_dirs={dirs}", self.tools),
            false => format!("model={model} fallback={} tools={} add_dirs={dirs}", self.fallback_model, self.tools),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub backend: Backend,
    pub timeout: Duration,
    pub reviewer: Reviewer,
    /// Set when a config file exists but could not be used: defaults are in
    /// effect and this string explains why (stderr + ERROR log + consult prefix).
    pub warning: Option<String>,
}

// ---- the TOML file schema (all fields optional; unknown keys rejected so a
// typo fails loudly instead of silently keeping the default) ----

#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    backend: Option<String>,
    timeout_secs: Option<u64>,
    ollama: Option<OllamaSection>,
    claude: Option<ClaudeSection>,
    openai: Option<OpenAiSection>,
    reviewer: Option<ReviewerSection>,
}

#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
struct OllamaSection {
    url: Option<String>,
    model: Option<String>,
    max_tokens: Option<u64>,
}

#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
struct ClaudeSection {
    bin: Option<String>,
    model: Option<String>,
    fallback_model: Option<String>,
}

#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
struct OpenAiSection {
    url: Option<String>,
    model: Option<String>,
    api_key: Option<String>,
    max_tokens: Option<u64>,
}

#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
struct ReviewerSection {
    model: Option<String>,
    fallback_model: Option<String>,
    tools: Option<String>,
    add_dirs: Option<Vec<String>>,
    timeout_secs: Option<u64>,
}

pub fn default_config() -> Config {
    Config {
        backend: Backend::Claude {
            bin: CLAUDE_BIN.to_string(),
            model: CLAUDE_MODEL.to_string(),
            fallback_model: CLAUDE_FALLBACK_MODEL.to_string(),
        },
        timeout: CLAUDE_TIMEOUT,
        reviewer: Reviewer {
            bin: CLAUDE_BIN.to_string(),
            model: CLAUDE_MODEL.to_string(),
            fallback_model: CLAUDE_FALLBACK_MODEL.to_string(),
            tools: REVIEWER_TOOLS.to_string(),
            add_dirs: Vec::new(),
            timeout: REVIEWER_TIMEOUT,
        },
        warning: None,
    }
}

// config_path: $ZCODE_CONSULTANT_CONFIG, else the OS-conventional config
// directory (Linux ~/.config honoring $XDG_CONFIG_HOME, macOS
// ~/Library/Application Support, Windows %APPDATA%).
pub fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var(ENV_CONFIG_PATH) {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    match dirs::config_dir() {
        Some(d) => d.join("zcode-consultant").join("config.toml"),
        None => util::home_dir().join(".config").join("zcode-consultant").join("config.toml"),
    }
}

// load reads the config file. File absent → defaults, no warning — except
// when the path came from the explicit env override, where a missing file is
// almost certainly a typo and gets a warning. Any other failure → defaults +
// warning (see the module comment).
pub fn load() -> Config {
    let override_set = std::env::var(ENV_CONFIG_PATH).map(|p| !p.is_empty()).unwrap_or(false);
    let path = config_path();
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if override_set {
                return with_warning(
                    &path,
                    "set via ZCODE_CONSULTANT_CONFIG but the file does not exist".to_string(),
                );
            }
            return default_config();
        }
        Err(e) => {
            return with_warning(&path, format!("cannot be read: {e}"));
        }
    };
    match from_toml_str(&raw) {
        Ok(c) => c,
        Err(e) => with_warning(&path, e),
    }
}

fn with_warning(path: &std::path::Path, problem: String) -> Config {
    let mut c = default_config();
    c.warning = Some(format!("config file {} ignored, defaults in use: {problem}", path.display()));
    c
}

// from_toml_str: parse + interpolate + validate. Pure (no filesystem), so
// tests can drive it directly.
pub fn from_toml_str(raw: &str) -> Result<Config, String> {
    // e.message() carries the parse error without the offending source line —
    // the line can be `api_key = "sk-..."` and the warning lands in logs, so
    // the raw text must not travel with it
    let f: FileConfig = toml::from_str(raw).map_err(|e| format!("invalid TOML: {}", e.message()))?;
    let timeout = f.timeout_secs.map(parse_timeout).transpose()?;
    // the reviewer defaults inherit [claude]'s bin/model (one CLI install for
    // both tools), so read them before the backend match consumes the section
    let (claude_bin, claude_model, claude_fallback) = match &f.claude {
        Some(s) => (s.bin.clone(), s.model.clone(), s.fallback_model.clone()),
        None => (None, None, None),
    };
    let rs = f.reviewer.unwrap_or_default();
    let reviewer = Reviewer {
        bin: interp_opt(claude_bin, CLAUDE_BIN)?,
        model: match rs.model {
            Some(m) => interpolate(&m)?,
            None => interp_opt(claude_model, CLAUDE_MODEL)?,
        },
        fallback_model: match rs.fallback_model {
            Some(m) => interpolate(&m)?,
            None => interp_opt(claude_fallback, CLAUDE_FALLBACK_MODEL)?,
        },
        tools: validate_reviewer_tools(&interp_opt(rs.tools, REVIEWER_TOOLS)?)?,
        add_dirs: rs
            .add_dirs
            .unwrap_or_default()
            .iter()
            .map(|d| interpolate(d))
            .collect::<Result<Vec<_>, _>>()?,
        timeout: rs.timeout_secs.map(parse_timeout).transpose()?.unwrap_or(REVIEWER_TIMEOUT),
    };
    let backend_name = f.backend.as_deref().unwrap_or("claude");
    let mut cfg = match backend_name {
        "ollama" => {
            let s = f.ollama.unwrap_or_default();
            let url = interp_opt(s.url, OLLAMA_URL)?;
            let model = interp_opt(s.model, OLLAMA_MODEL)?;
            let max_tokens = s.max_tokens.unwrap_or(OLLAMA_MAX_TOKENS);
            validate_max_tokens(max_tokens)?;
            Config { backend: Backend::Ollama { url, model, max_tokens }, timeout: OLLAMA_TIMEOUT, reviewer, warning: None }
        }
        "claude" => {
            let s = f.claude.unwrap_or_default();
            let bin = interp_opt(s.bin, CLAUDE_BIN)?;
            let model = interp_opt(s.model, CLAUDE_MODEL)?;
            let fallback_model = interp_opt(s.fallback_model, CLAUDE_FALLBACK_MODEL)?;
            Config { backend: Backend::Claude { bin, model, fallback_model }, timeout: CLAUDE_TIMEOUT, reviewer, warning: None }
        }
        "openai" => {
            let s = f.openai.ok_or("backend \"openai\" requires an [openai] section")?;
            let url = interp_req(s.url, "[openai] url")?;
            let model = interp_req(s.model, "[openai] model")?;
            let api_key = interp_req(s.api_key, "[openai] api_key")?;
            let max_tokens = s.max_tokens.unwrap_or(OPENAI_MAX_TOKENS);
            validate_max_tokens(max_tokens)?;
            Config {
                backend: Backend::OpenAi { url, model, api_key, max_tokens },
                timeout: OPENAI_TIMEOUT,
                reviewer,
                warning: None,
            }
        }
        other => return Err(format!("unknown backend \"{}\" (expected ollama, claude, or openai)", util::truncate(other, 40))),
    };
    if let Some(t) = timeout {
        cfg.timeout = t;
    }
    Ok(cfg)
}

// parse_timeout: shared validation for timeout_secs (global and reviewer).
fn parse_timeout(s: u64) -> Result<Duration, String> {
    match s {
        0 => Err("timeout_secs must be > 0".into()),
        // a bogus huge value would overflow Instant arithmetic mid-consult
        // (and panic the hook process); one day is far past any real deadline
        s if s > 86_400 => Err("timeout_secs must be ≤ 86400 (one day)".into()),
        s => Ok(Duration::from_secs(s)),
    }
}

// validate_reviewer_tools: hard whitelist — the tool description promises a
// read-only reviewer running under the user's CLI auth, so a typo like
// "Read,Bash" must fail at config load, not mid-review.
fn validate_reviewer_tools(raw: &str) -> Result<String, String> {
    const ALLOWED: [&str; 3] = ["Read", "Grep", "Glob"];
    let mut out = Vec::new();
    for t in raw.split(',') {
        let t = t.trim();
        if t.is_empty() {
            continue;
        }
        if !ALLOWED.contains(&t) {
            return Err(format!(
                "reviewer tools: \"{t}\" is not allowed (whitelist: Read, Grep, Glob — the reviewer must stay read-only)"
            ));
        }
        out.push(t.to_string());
    }
    if out.is_empty() {
        return Err("reviewer tools: empty tool set".into());
    }
    Ok(out.join(","))
}

fn validate_max_tokens(v: u64) -> Result<(), String> {
    if v < 256 {
        return Err(format!("max_tokens={v} is too small (a reasoning model's thinking counts toward it; use ≥ 4096)"));
    }
    Ok(())
}

fn interp_opt(v: Option<String>, default: &str) -> Result<String, String> {
    match v {
        None => Ok(default.to_string()),
        Some(s) => interpolate(&s),
    }
}

fn interp_req(v: Option<String>, field: &str) -> Result<String, String> {
    let s = v.ok_or_else(|| format!("{field} is required for this backend"))?;
    let s = interpolate(&s)?;
    if s.trim().is_empty() {
        return Err(format!("{field} is empty"));
    }
    Ok(s)
}

// interpolate expands ${VAR} / ${VAR:-fallback} / $$ in one pass. A "$" not
// followed by "{" or "$" is a literal "$".
pub fn interpolate(s: &str) -> Result<String, String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        if let Some(tail) = after.strip_prefix('$') {
            out.push('$');
            rest = tail;
        } else if let Some(body) = after.strip_prefix('{') {
            let j = body
                .find('}')
                .ok_or_else(|| "unterminated ${ placeholder (missing closing '}')".to_string())?;
            let expr = &body[..j];
            let (name, fallback) = match expr.split_once(":-") {
                Some((n, fb)) => (n, Some(fb)),
                None => (expr, None),
            };
            if !valid_var_name(name) {
                return Err(format!("invalid environment variable name {:?}", util::truncate(name, 40)));
            }
            match std::env::var(name) {
                Ok(v) if !v.is_empty() => out.push_str(&v),
                Ok(_) => match fallback {
                    // bash :- semantics: an empty value counts as unset
                    Some(fb) => out.push_str(fb),
                    None => return Err(format!("environment variable {name} is set but empty")),
                },
                Err(_) => match fallback {
                    Some(fb) => out.push_str(fb),
                    None => return Err(format!("environment variable {name} is not set (referenced in config)")),
                },
            }
            rest = &body[j + 1..];
        } else {
            out.push('$');
            rest = after;
        }
    }
    out.push_str(rest);
    Ok(out)
}

fn valid_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

// global: lazily loaded once per process. Hook mode and the MCP server both
// go through here; tests avoid this and drive the pure functions instead (a
// developer's real config file must not make unit tests nondeterministic).
pub fn global() -> &'static Config {
    static CONFIG: OnceLock<Config> = OnceLock::new();
    CONFIG.get_or_init(load)
}

impl Backend {
    /// Short redacted identifier for logs and the tool description — never
    /// includes the api_key; for openai it names the host, not the full URL
    /// (which could carry a token in a query string).
    pub fn kind_and_summary(&self) -> String {
        match self {
            Backend::Ollama { url, model, max_tokens } => {
                format!("ollama url={url} model={model} max_tokens={max_tokens}")
            }
            Backend::Claude { model, fallback_model, .. } => {
                let m = if model.trim().is_empty() { "<cli-default>".to_string() } else { model.clone() };
                match fallback_model.trim().is_empty() {
                    true => format!("claude model={m}"),
                    false => format!("claude model={m} fallback={fallback_model}"),
                }
            }
            Backend::OpenAi { url, model, max_tokens, .. } => {
                format!("openai url={} model={model} max_tokens={max_tokens} api_key=<redacted>", host_of(url))
            }
        }
    }
}

// host_of: scheme + path + userinfo stripped — the displayable identity of an
// endpoint URL (https://user:pass@host/path?token=x → "host").
pub fn host_of(url: &str) -> &str {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let authority = rest.split('/').next().unwrap_or(rest);
    authority.rsplit('@').next().unwrap_or(authority)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- interpolation ----

    #[test]
    fn interpolation_rules() {
        // unique names so parallel tests can't collide
        std::env::set_var("ZCA_TEST_VAR", "hello");
        std::env::set_var("ZCA_TEST_EMPTY", "");
        std::env::set_var("ZCA_TEST_DIR", "/interp-worked");
        std::env::remove_var("ZCA_TEST_UNSET");

        assert_eq!(interpolate("plain").unwrap(), "plain");
        assert_eq!(interpolate("${ZCA_TEST_VAR}").unwrap(), "hello");
        assert_eq!(interpolate("a-${ZCA_TEST_VAR}-b").unwrap(), "a-hello-b");
        assert_eq!(interpolate("$$var").unwrap(), "$var");
        assert_eq!(interpolate("100$").unwrap(), "100$"); // lone $ is literal
        assert_eq!(interpolate("${ZCA_TEST_VAR:-fb}").unwrap(), "hello");
        assert_eq!(interpolate("${ZCA_TEST_EMPTY:-fb}").unwrap(), "fb"); // empty counts as unset
        assert_eq!(interpolate("${ZCA_TEST_UNSET:-fb}").unwrap(), "fb");
        assert_eq!(interpolate("${ZCA_TEST_UNSET:-}").unwrap(), ""); // explicit empty fallback
        assert!(interpolate("${ZCA_TEST_UNSET}").is_err());
        assert!(interpolate("${ZCA_TEST_EMPTY}").is_err());
        assert!(interpolate("${ZCA_TEST_UNSET").is_err()); // unterminated
        assert!(interpolate("${1BAD}").is_err()); // bad name
        // single-pass: the fallback is literal text — never re-expanded or
        // re-unescaped (a fallback containing "}" would also terminate the
        // expression early; not supported, document instead of nesting)
        assert_eq!(interpolate("${ZCA_TEST_UNSET:-a$$b}").unwrap(), "a$$b");
        assert_eq!(interpolate("x${ZCA_TEST_UNSET:-${ZCA_TEST_VAR}}y").unwrap(), "x${ZCA_TEST_VAR}y");
    }

    // ---- parsing ----

    #[test]
    fn empty_string_is_valid_defaults() {
        let c = from_toml_str("").unwrap();
        assert!(matches!(c.backend, Backend::Claude { ref model, ref fallback_model, .. }
            if model == CLAUDE_MODEL && fallback_model == CLAUDE_FALLBACK_MODEL));
        assert_eq!(c.timeout, CLAUDE_TIMEOUT);
        assert!(c.warning.is_none());
    }

    #[test]
    fn full_file_parses() {
        let c = from_toml_str(
            r#"
            backend = "openai"
            timeout_secs = 60
            [ollama]
            url = "http://127.0.0.1:11434/v1/chat/completions"
            model = "mistral"
            max_tokens = 4096
            [claude]
            bin = "/usr/local/bin/claude"
            model = "sonnet"
            [openai]
            url = "https://api.example.com/v1/chat/completions"
            model = "glm-4.7"
            api_key = "sk-test"
            max_tokens = 16384
            "#,
        )
        .unwrap();
        match c.backend {
            Backend::OpenAi { url, model, api_key, max_tokens } => {
                assert_eq!(url, "https://api.example.com/v1/chat/completions");
                assert_eq!(model, "glm-4.7");
                assert_eq!(api_key, "sk-test");
                assert_eq!(max_tokens, 16384);
            }
            other => panic!("wrong backend: {other:?}"),
        }
        assert_eq!(c.timeout, Duration::from_secs(60));
    }

    #[test]
    fn claude_defaults_adopt_higher_timeout() {
        let c = from_toml_str("backend = \"claude\"").unwrap();
        assert!(matches!(c.backend, Backend::Claude { ref bin, .. } if bin == CLAUDE_BIN));
        assert_eq!(c.timeout, CLAUDE_TIMEOUT);
    }

    #[test]
    fn fallback_model_parses_and_inherits() {
        // default: the built-in chain fable→opus
        let c = from_toml_str("").unwrap();
        assert_eq!(c.reviewer.fallback_model, CLAUDE_FALLBACK_MODEL);
        // [claude] fallback flows into the backend and the reviewer (same
        // inheritance as bin/model)
        let c = from_toml_str("backend = \"claude\"\n[claude]\nmodel = \"fable\"\nfallback_model = \"opus\"").unwrap();
        match &c.backend {
            Backend::Claude { model, fallback_model, .. } => {
                assert_eq!(model, "fable");
                assert_eq!(fallback_model, "opus");
            }
            other => panic!("wrong backend: {other:?}"),
        }
        assert_eq!(c.reviewer.fallback_model, "opus");
        // [reviewer] override wins over inheritance
        let c = from_toml_str(
            "backend = \"claude\"\n[claude]\nfallback_model = \"opus\"\n[reviewer]\nfallback_model = \"sonnet\"",
        )
        .unwrap();
        assert_eq!(c.reviewer.fallback_model, "sonnet");
        assert!(matches!(&c.backend, Backend::Claude { fallback_model, .. } if fallback_model == "opus"));
        // summary carries the chain without secrets
        let s = c.backend.kind_and_summary();
        assert!(s.contains("fallback=opus"), "{s}");
    }

    #[test]
    fn reviewer_defaults_and_inheritance() {
        // no config: read-only whitelist, 600s, and the claude default chain
        // (reviewer inherits [claude]'s model/fallback → fable/opus)
        let c = from_toml_str("").unwrap();
        assert_eq!(c.reviewer.tools, REVIEWER_TOOLS);
        assert_eq!(c.reviewer.timeout, REVIEWER_TIMEOUT);
        assert_eq!(c.reviewer.bin, CLAUDE_BIN);
        assert_eq!(c.reviewer.model, CLAUDE_MODEL);
        assert_eq!(c.reviewer.fallback_model, CLAUDE_FALLBACK_MODEL);
        // [claude] section is inherited even when the advisor backend is ollama
        let c = from_toml_str("backend = \"ollama\"\n[claude]\nbin = \"/opt/claude\"\nmodel = \"sonnet\"").unwrap();
        assert_eq!(c.reviewer.bin, "/opt/claude");
        assert_eq!(c.reviewer.model, "sonnet");
        // [reviewer] overrides win over inheritance
        std::env::set_var("ZCA_TEST_DIR", "/interp-worked"); // don't rely on interpolation_rules' env (parallel tests)
        let c = from_toml_str(
            "backend = \"claude\"\n[claude]\nmodel = \"sonnet\"\n[reviewer]\nmodel = \"opus\"\ntimeout_secs = 60\nadd_dirs = [\"/tmp/probe\", \"${ZCA_TEST_DIR}\"]",
        )
        .unwrap();
        assert_eq!(c.reviewer.model, "opus");
        assert_eq!(c.reviewer.timeout, Duration::from_secs(60));
        assert_eq!(c.reviewer.add_dirs, vec!["/tmp/probe", "/interp-worked"]);
        // fallback_model unset anywhere → inherits the built-in opus default
        assert_eq!(
            c.reviewer.summary(),
            "model=opus fallback=opus tools=Read,Grep,Glob add_dirs=/tmp/probe,/interp-worked"
        );
    }

    #[test]
    fn reviewer_tools_are_whitelist_validated() {
        // the tool description promises a read-only reviewer — non-read-only
        // tools must fail at config load, naming the offender
        let err = from_toml_str("[reviewer]\ntools = \"Read,Bash\"").unwrap_err();
        assert!(err.contains("Bash") && err.contains("not allowed"), "{err}");
        assert!(from_toml_str("[reviewer]\ntools = \"Read, Write\"").is_err());
        assert!(from_toml_str("[reviewer]\ntools = \"read\"").is_err()); // case-sensitive
        assert!(from_toml_str("[reviewer]\ntools = \"\"").unwrap_err().contains("empty"));
        // whitelist accepted in any order/duplication-free form
        let c = from_toml_str("[reviewer]\ntools = \"Glob, Read\"").unwrap();
        assert_eq!(c.reviewer.tools, "Glob,Read");
        assert!(from_toml_str("[reviewer]\ntimeout_secs = 0").is_err());
        assert!(from_toml_str("[reviewer]\ntimeout_secs = 99999999999").is_err());
    }

    #[test]
    fn broken_files_are_rejected_with_a_named_error() {
        assert!(from_toml_str("backend = \"nope\"").unwrap_err().contains("unknown backend"));
        assert!(from_toml_str("what = 1").unwrap_err().contains("unknown field")); // deny_unknown_fields
        assert!(from_toml_str("timeout_secs = 0").unwrap_err().contains("timeout_secs"));
        assert!(from_toml_str("timeout_secs = 99999999999999").unwrap_err().contains("86400"));
        assert!(from_toml_str("backend = \"openai\"").unwrap_err().contains("[openai] section"));
        assert!(from_toml_str("backend = \"openai\"\n[openai]\nmodel = \"m\"\napi_key = \"k\"")
            .unwrap_err()
            .contains("url"));
        assert!(from_toml_str("not even toml [[[").is_err());
        // a secret in the broken line must not travel with the error text
        let err = from_toml_str("what = 1\napi_key = \"sk-super-secret-123\"").unwrap_err();
        assert!(!err.contains("sk-super-secret"), "leaked: {err}");
        // an [ollama] section is only validated when it's the chosen backend
        // (the default is claude) — select it explicitly to exercise the path
        let err = from_toml_str("backend = \"ollama\"\n[ollama]\nmodel = \"${unterminated\"").unwrap_err();
        assert!(err.contains("unterminated"), "{err}");
        assert!(!err.contains("${unterminated"), "raw value leaked: {err}");
        assert!(
            from_toml_str(
                "backend = \"openai\"\n[openai]\nurl = \"https://x/v1\"\nmodel = \"m\"\napi_key = \"${ZCA_DEFINITELY_UNSET_VAR}\""
            )
            .unwrap_err()
            .contains("ZCA_DEFINITELY_UNSET_VAR")
        );
        assert!(from_toml_str("backend = \"ollama\"\n[ollama]\nmax_tokens = 8").unwrap_err().contains("too small"));
    }

    #[test]
    fn host_of_strips_scheme_path_and_userinfo() {
        assert_eq!(host_of("https://api.example.com/v1/chat"), "api.example.com");
        assert_eq!(host_of("http://localhost:11434/v1"), "localhost:11434");
        assert_eq!(host_of("https://user:pass@host/path?token=x"), "host");
        assert_eq!(host_of("no-scheme.example/path"), "no-scheme.example");
    }

    #[test]
    fn env_override_missing_file_warns() {
        let missing = std::env::temp_dir().join(format!("zca-no-such-config-{}", std::process::id()));
        let _ = std::fs::remove_file(&missing);
        std::env::set_var(ENV_CONFIG_PATH, &missing);
        let c = load();
        assert!(c.warning.as_deref().unwrap_or_default().contains("does not exist"), "{c:?}");
        // an existing file (empty parses as pure defaults) → no warning; the
        // assertion must not depend on the developer's real config file
        let existing = std::env::temp_dir().join(format!("zca-empty-config-{}", std::process::id()));
        std::fs::write(&existing, "").unwrap();
        std::env::set_var(ENV_CONFIG_PATH, &existing);
        assert!(load().warning.is_none());
        std::env::remove_var(ENV_CONFIG_PATH);
        let _ = std::fs::remove_file(&existing);
    }

    #[test]
    fn interpolation_applies_to_fields() {
        std::env::set_var("ZCA_TEST_KEY", "sk-live-123");
        let c = from_toml_str(
            "backend = \"openai\"\n[openai]\nurl = \"https://x/v1\"\nmodel = \"m\"\napi_key = \"${ZCA_TEST_KEY}\"",
        )
        .unwrap();
        match c.backend {
            Backend::OpenAi { api_key, .. } => assert_eq!(api_key, "sk-live-123"),
            other => panic!("wrong backend: {other:?}"),
        }
    }

    #[test]
    fn summary_is_redacted() {
        let c = from_toml_str(
            "backend = \"openai\"\n[openai]\nurl = \"https://x/v1\"\nmodel = \"m\"\napi_key = \"sk-secret\"",
        )
        .unwrap();
        let s = c.backend.kind_and_summary();
        assert!(s.contains("<redacted>"), "{s}");
        assert!(!s.contains("sk-secret"), "{s}");
    }
}
