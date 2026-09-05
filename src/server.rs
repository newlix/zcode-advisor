// advisor 是一個極簡的 MCP stdio server：向 ZCode 的主模型（executor）提供
// consult_advisor 工具，讓更強的顧問模型給出戰略建議。顧問只有一個後端：本機
// Ollama 的雲端託管模型（走 Ollama 帳號計費，非本地 GGUF），模型與端點寫死在
// 下方常數——要換就改這裡重編。
// 對應 Anthropic advisor tool 的精神：顧問失敗時降級放行、輸出有上限、時機由主模型自己決定。
// 協議層使用官方 rmcp SDK；consult 的阻塞工作（rollout 反查 + HTTP）經
// spawn_blocking 執行，並以 Arc<Mutex> 串行化——對應舊手寫迴圈「一次一個 consult」
// 的行為，也避免狀態檔並發寫（rmcp 會並發 dispatch 請求）。

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rmcp::{
    ErrorData as McpError,
    handler::server::wrapper::Parameters,
    model::*,
    schemars,
    tool, tool_handler, tool_router,
    ServerHandler, ServiceExt,
};

use crate::{hooks, http, logger, rollout};

pub const SERVER_NAME: &str = env!("CARGO_PKG_NAME"); // 單一事實來源：Cargo.toml
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
// 顧問模型設定（唯一事實來源）：kimi-k3:cloud 是 Ollama 的雲端託管模型，
// 需先 ollama login 且本機 ollama serve 在跑；ollama pull 拉不下來、
// /api/tags 也看不到它。換模型或端點改這兩行重編。
pub const ADVISOR_MODEL: &str = "kimi-k3:cloud";
pub const ADVISOR_URL: &str = "http://localhost:11434/v1/chat/completions";

// 顧問輸出上限（kimi-k3 建議值）。reasoning 模型（kimi-k3 等）的推理文字計入
// max_tokens：預算太小（如 2048）會在推理階段就燒光、正文為空；max_tokens 是
// 上限不是預留，簡單問題成本不變
pub const MAX_TOKENS: u64 = 131072;
pub const MAX_USES: i32 = 0; // MCP 工具每 session 呼叫上限；0 = 不限次數

pub const ADVISOR_SYSTEM_PROMPT: &str = "You are a senior engineering advisor consulted by a coding agent mid-task. Answer with concise, actionable advice: key risks, recommended approach, and how to verify. Under 300 words. Do not restate the question. Plain text only.";

static USE_COUNT: AtomicI32 = AtomicI32::new(0); // MAX_USES 計數（consult 已串行化，atomic 僅示意）

// 在飛 consult 標記：EOF 後若 shutdown 把 consult 半途收掉，run_server 據此留一行
// stderr 供事後追查「為什麼這次沒有建議」（degradation 路徑不可完全靜默）
static CONSULT_IN_FLIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

struct InFlightGuard;
impl Drop for InFlightGuard {
    fn drop(&mut self) {
        CONSULT_IN_FLIGHT.store(false, Ordering::SeqCst);
    }
}

pub fn advisor_label() -> String {
    format!("{ADVISOR_MODEL} via Ollama")
}

#[derive(Clone)]
pub struct Advisor {
    // Arc 確保 rmcp 若 clone handler 仍共享同一把鎖；consult 全程（反查→標記→HTTP）串行。
    // （tool router 不存 field：#[tool_handler] 產生的 call_tool/list_tools 走
    // Self::tool_router() 重建，表的建構成本可忽略。）
    consult_lock: Arc<tokio::sync::Mutex<()>>,
}

/// 欄位 doc comment 會成為參數 description（schemars），文案沿用舊版逐字不動。
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ConsultArgs {
    /// What you want the advisor to decide or advise on.
    pub question: String,
    /// Optional supporting material: relevant code, error messages, or a summary of attempts so far.
    #[serde(default)]
    pub context: Option<String>,
}

#[tool_router]
impl Advisor {
    pub fn new() -> Self {
        Self {
            consult_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    // description 文案沿用舊版逐字不動——它決定 ZCode 何時、如何呼叫這個工具
    #[tool(description = "Consult a stronger advisor model (kimi-k3:cloud via Ollama) for strategic guidance. Use it when starting a complex or unfamiliar task, before a large/risky change, when stuck after failed attempts, or when unsure about the approach. Your current conversation is attached automatically — focus the question on what you need decided, and use the optional context field only for material not yet in the conversation.")]
    async fn consult_advisor(
        &self,
        Parameters(ConsultArgs { question, context }): Parameters<ConsultArgs>,
    ) -> Result<CallToolResult, McpError> {
        if question.trim().is_empty() {
            logger::info("consult rejected reason=empty-question");
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "error: 'question' is required and must be non-empty",
            )]));
        }
        let context = context.unwrap_or_default();
        let _guard = self.consult_lock.lock().await;
        // rollout 反查（掃檔）與 HTTP（90s deadline）都是阻塞 IO：丟 blocking pool，
        // 不占 runtime 線程；panic 由 JoinError 接住，降級為 caller 可見的錯誤結果
        let res = tokio::task::spawn_blocking(move || consult(&question, &context)).await;
        Ok(res.unwrap_or_else(|e| {
            logger::info(&format!("consult failed reason=task-panic err={e}"));
            advice_error(&format!("error: consult task failed: {e}"))
        }))
    }
}

#[tool_handler]
impl ServerHandler for Advisor {
    // 只聲明 tools 能力、不加 instructions——與舊版廣告的能力面一致
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(SERVER_NAME, SERVER_VERSION))
    }
}

pub fn run_server() {
    logger::init("server");
    eprintln!("advisor: model={ADVISOR_MODEL} url={ADVISOR_URL}"); // 日誌一律走 stderr，stdout 只有 MCP 協議
    logger::info(&format!(
        "server started version={SERVER_VERSION} model={ADVISOR_MODEL} url={ADVISOR_URL}"
    ));
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("advisor: runtime init failed: {e}");
            logger::error(&format!("runtime init failed err={e}"));
            return;
        }
    };
    let result = rt.block_on(async {
        let service = Advisor::new().serve(rmcp::transport::stdio()).await?;
        service.waiting().await?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    // waiting() 已含 rmcp 的 5s 排水窗；這裡再給 5s 等殘餘 blocking consult 收尾。
    // 不做的話 runtime drop 會無限等在飛的 HTTP（最多 90s deadline）——client 已斷，
    // 回應無處可去，不值得陪等。
    // 順序依賴：mark_consulted 在 HTTP 之前執行，因此中途被收掉不會留下半寫的
    // 狀態檔；若未來把狀態寫入挪到 HTTP 之後，這裡的「安全截斷」保證即失效。
    rt.shutdown_timeout(Duration::from_secs(5));
    if CONSULT_IN_FLIGHT.load(Ordering::SeqCst) {
        eprintln!("advisor: shutdown cut an in-flight consult (client gone; state writes happen before HTTP, nothing half-written)");
        logger::info("shutdown cut an in-flight consult (client gone; state writes happen before HTTP, nothing half-written)");
    }
    if let Err(e) = result {
        eprintln!("advisor: server error: {e}");
        logger::error(&format!("server error err={e:?}"));
    }
}

// consult 呼叫顧問模型（在 blocking 線程上執行）。任何失敗都以 is_error 的工具
// 結果回傳（主模型看得到、可自行繼續），絕不讓整個 server 掛掉——對應原版
// advisor「顧問失敗不得讓任務失敗」的設計。
fn consult(question: &str, context_str: &str) -> CallToolResult {
    let _in_flight = InFlightGuard;
    CONSULT_IN_FLIGHT.store(true, Ordering::SeqCst);
    let started = std::time::Instant::now();
    logger::info(&format!("consult question={:?}", crate::util::truncate(question, 48)));
    if MAX_USES > 0 && USE_COUNT.fetch_add(1, Ordering::SeqCst) + 1 > MAX_USES {
        logger::info("consult rejected reason=budget-exhausted");
        return advice_error(&format!(
            "error: advice budget exhausted (max {MAX_USES} consults for this session); proceed on your own"
        ));
    }
    // 反查呼叫端 session（UUID 級）：自動帶入該 session 的對話作為顧問 context
    let mut advice_context = context_str.to_string();
    let mut session_note = String::new();
    let mut matched_sess = String::from("-");
    if let Some(m) = rollout::find_calling_session(question) {
        eprintln!("advisor: rollout match: session={} file={}", m.session_id, m.path.display());
        logger::info(&format!("rollout match session={} file={}", m.session_id, m.path.display()));
        matched_sess.clone_from(&m.session_id);
        hooks::mark_consulted(&m.session_id); // 已諮詢：開場提醒就此收聲
        if !m.dialog.is_empty() || !m.preamble.is_empty() {
            let mut b = String::new();
            if !m.preamble.is_empty() {
                // 當前輪獨白：executor 來問之前的想法，顧問最該先看
                b.push_str(&format!("The agent's words immediately before calling you:\n{}\n\n", m.preamble));
            }
            if !m.dialog.is_empty() {
                b.push_str(&format!(
                    "The calling agent's current conversation (system prompt omitted, oldest first, may be truncated):\n{}",
                    m.dialog
                ));
            }
            if !context_str.trim().is_empty() {
                advice_context = format!("{b}\n--- additional context from the agent ---\n{context_str}");
            } else {
                advice_context = b;
            }
        }
        // session id（sess_<uuid>）前 8 碼標註歸屬；非 ASCII 時 from_utf8_lossy 對應
        // 無效 UTF-8 的替換行為
        let bytes = m.session_id.as_bytes();
        let short = if bytes.len() > 8 {
            String::from_utf8_lossy(&bytes[..8]).into_owned()
        } else {
            m.session_id.clone()
        };
        session_note = format!(" | session {short}");
    }
    // 「顧問看到什麼」的內容（完整 question、對話、建議）由 ZCode 的 rollout 檔
    // 原生保存，日誌只記結構軌跡
    match ask_advisor(question, &advice_context) {
        Err(e) => {
            logger::info(&format!(
                "consult failed sess={matched_sess} t={:?} ctx={}B err={e}",
                started.elapsed(),
                advice_context.len()
            ));
            advice_error(&format!("error: {e}"))
        }
        Ok(advice) => {
            logger::info(&format!(
                "consult done sess={matched_sess} t={:?} ctx={}B advice={}B",
                started.elapsed(),
                advice_context.len(),
                advice.len()
            ));
            CallToolResult::success(vec![ContentBlock::text(format!(
                "[advisor · {}{session_note}]\n{advice}",
                advisor_label()
            ))])
        }
    }
}

// ask_advisor 打本機 Ollama 的 OpenAI 相容端點。MCP 工具與 hook 模式共用，
// 任何錯誤以 Err 回傳，由呼叫端決定呈現方式（MCP 回 is_error 結果、hook 靜默放行）。
pub fn ask_advisor(question: &str, context_str: &str) -> Result<String, String> {
    let mut user_msg = question.to_string();
    if !context_str.trim().is_empty() {
        user_msg.push_str("\n\n--- context ---\n");
        user_msg.push_str(context_str);
    }
    let payload = serde_json::json!({
        "messages": [
            {"content": ADVISOR_SYSTEM_PROMPT, "role": "system"},
            {"content": user_msg, "role": "user"},
        ],
        "model": ADVISOR_MODEL,
        "max_tokens": MAX_TOKENS,
        "temperature": 0.3,
    });
    let body = serde_json::to_string(&payload).map_err(|e| format!("encode request: {e}"))?;

    let http::HttpResponse { status, body: resp_body } = http::post_json(ADVISOR_URL, &body, Duration::from_secs(90))
        .map_err(|e| format!("advisor API unreachable: {e}"))?;

    // 先解 body 再看 status——錯誤頁（非 JSON）會走到 unreadable body；
    // Decoder 只取第一個 JSON 值、容忍尾隨資料（json.Decoder 的語義）。
    let data = decode_advisor_body(&resp_body)
        .map_err(|e| format!("advisor API returned HTTP {status} with unreadable body: {e}"))?;

    if status != 200 {
        let mut msg = format!("HTTP {status}");
        if let Some(err) = &data.error {
            let m = err.message.as_deref().unwrap_or("");
            if !m.is_empty() {
                msg.push_str(&format!(": {m}"));
            }
        }
        return Err(format!("advisor API failed: {msg}"));
    }
    let choice = data.choices.and_then(|c| c.into_iter().next());
    let content = choice
        .as_ref()
        .and_then(|c| c.message.as_ref())
        .and_then(|m| m.content.as_deref())
        .unwrap_or("")
        .to_string();
    if !content.is_empty() {
        // finish=length 但正文非空＝被截斷的答案：照常回傳，呼叫端無從分辨（可接受，勿誤當完整）
        return Ok(content);
    }
    if choice.as_ref().and_then(|c| c.finish_reason.as_deref()) == Some("length") {
        // reasoning 模型推理計入 max_tokens：length + 空正文＝預算在推理階段耗盡
        return Err(format!(
            "advisor spent all max_tokens={MAX_TOKENS} on reasoning before answering (finish_reason=length); raise the MAX_TOKENS constant in source and rebuild"
        ));
    }
    Err("advisor returned an empty response".into())
}

// Ollama OpenAI 相容端點的回應。Go 的 encoding/json 把 JSON null 視為 no-op
// （欄位保零值），serde 的 default 只 cover「缺欄」——因此每個欄位用 Option 接
// null、unwrap_or_default 還原零值語義；這對 finish_reason:null（OpenAI 相容
// 回應常見）尤其重要。
#[derive(serde::Deserialize, Default)]
struct AdvisorResp {
    #[serde(default)]
    choices: Option<Vec<Choice>>,
    #[serde(default)]
    error: Option<ErrorObj>,
}

#[derive(serde::Deserialize, Default)]
struct Choice {
    #[serde(default)]
    message: Option<Message>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(serde::Deserialize, Default)]
struct Message {
    #[serde(default)]
    content: Option<String>,
}

#[derive(serde::Deserialize, Default)]
struct ErrorObj {
    #[serde(default)]
    message: Option<String>,
}

// decode_advisor_body：只取第一個 JSON 值、容忍尾隨資料；頂層 null 對映回零值
// （Go 語義）；空 body（EOF）→ Err，走 unreadable body 路徑。
fn decode_advisor_body(body: &[u8]) -> Result<AdvisorResp, String> {
    serde_json::Deserializer::from_slice(body)
        .into_iter::<Option<AdvisorResp>>()
        .next()
        .transpose()
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "unexpected end of input".to_string())
        .map(|opt| opt.unwrap_or_default())
}

fn advice_error(msg: &str) -> CallToolResult {
    // rmcp 的 tool-level error：caller（主模型）看得到內文，正合顧問降級語義
    CallToolResult::error(vec![ContentBlock::text(msg.to_string())])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_router_registers_consult_advisor() {
        let router = Advisor::tool_router();
        assert!(router.has_route("consult_advisor"));
        let tools = router.list_all();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name.to_string(), "consult_advisor");
        let desc = tools[0].description.as_deref().unwrap_or("");
        assert!(desc.contains("kimi-k3:cloud via Ollama"), "{desc}");
        assert!(desc.contains("Your current conversation is attached automatically"), "{desc}");
    }

    #[test]
    fn advisor_body_decodes_null_semantics() {
        let d = decode_advisor_body(r#"{"choices":[{"message":{"content":"ok"},"finish_reason":null}]}"#.as_bytes()).unwrap();
        let choices = d.choices.unwrap();
        assert_eq!(
            choices.first().and_then(|c| c.message.as_ref()).and_then(|m| m.content.as_deref()),
            Some("ok")
        );
        let d = decode_advisor_body(r#"{"choices":[{"message":null,"finish_reason":null}],"error":null}"#.as_bytes()).unwrap();
        assert!(d.choices.unwrap()[0].message.is_none());
        let d = decode_advisor_body(b"null").unwrap();
        assert!(d.choices.is_none());
        assert!(decode_advisor_body(b"").is_err());
        assert!(decode_advisor_body(r#"{"choices":[{"message":{"content":42}}]}"#.as_bytes()).is_err());
        assert!(decode_advisor_body(b"{\"a\":1} trailing garbage").is_ok());
    }
}
