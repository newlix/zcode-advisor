# zcode-advisor

ZCode 版的 [Anthropic advisor tool](https://platform.claude.com/docs/en/agents-and-tools/tool-use/advisor-tool)：讓便宜快速的執行模型（glm-5.3-flash）在關鍵時刻向更強的顧問模型徵詢戰略建議（顧問固定為本機 Ollama 的 `kimi-k3:cloud`，模型與端點寫死在 `src/server.rs` 常數）。單一 binary、兩種模式（MCP stdio server / hook）。

**MCP 協議層使用官方 [rmcp](https://crates.io/crates/rmcp) SDK**（spec 2026-07-28 慣例：`#[tool_router]`/`#[tool_handler]`、tokio current-thread runtime、stdio transport）。advisor 功能（顧問降級語義、輸出上限、時機教育）與 Go 版（`~/.zcode/zcode-advisor`，已退役）等義；hook 的輸出格式（提醒文字、additionalContext）與 rollout 反查逐位元組一致，但執行期資料（state、日誌）已移至跨平台資料目錄、與 Go 遺留目錄分家（見「記錄與追查」）。

## 系統組成

一個 binary，兩種模式，三個觸發點：

| 觸發點 | 機制 | 時機由誰決定 |
|---|---|---|
| `consult_advisor` 工具（MCP） | 主模型呼叫，自動附完整對話視野＋當前輪獨白 | 主模型自己 |
| 諮詢提醒（`UserPromptSubmit` hook） | prompt 有份量且尚未諮詢過時注入一行提醒，**不打 API** | 規則（原版 nudge 的對應物） |
| 卡關診斷（`PostToolUseFailure` hook） | 連續失敗 ≥2 次才諮詢，5 分鐘冷卻、每 session 上限 5 次 | 規則 |

另有 `PostToolUse` hook（`hook PostToolUseOK`）只重置連續失敗計數，零成本。

設計鐵律：**顧問缺席不能耽誤正事**——API 失敗、Ollama 沒開、額度用盡時，MCP 工具回 caller 可見的 `isError` 結果（rmcp 的 tool-level error）、hook 靜默放行，任何路徑都不擋任務。

## MCP 層（rmcp）

- `Advisor` 實作 `ServerHandler`，只聲明 tools 能力；`consult_advisor` 由 `#[tool(description = ...)]` 註冊，參數 schema 由 schemars 從 `ConsultArgs` 生成（`question` 必填、`context` 選填，description 文案沿用舊版）。
- consult 的阻塞工作（rollout 反查、90s HTTP）經 `tokio::task::spawn_blocking` 執行；`Arc<Mutex>` 將 consult 全程串行——rmcp 會並發 dispatch 請求，串行化對應舊版單執行緒迴圈「一次一個 consult」的行為，也避免狀態檔並發寫。
- blocking task 內 panic 由 `JoinError` 接住、降級為 isError 結果；顧問端點不可達/逾時/空回應的錯誤訊息與判斷邏輯（`finish_reason=length` 偵測等）沿用舊版。
- 協議層細節（JSON-RPC id 回寫、notification 處理、版本協商）由 rmcp 處理：client 送的 `protocolVersion` 在支援集合內 echo（實測 2024-11-05 / 2025-03-26 / 2025-06-18 皆 echo），不認得的版本回 rmcp 最新版。

## UUID 級 session 歸屬（核心技巧）

Anthropic 原版的 advisor 在 provider 竊聽不到的地方跑，自動拿到完整對話；MCP server 只看得到 stdio。本專案的解法：**rollout 反查**。

ZCode 為每個 session 在 `~/.zcode/cli/rollout/model-io-sess_<uuid>.jsonl` 記錄每一輪完整的模型請求/回應。模型決定呼叫 `consult_advisor` 的那個 response **必然在工具執行前落盤**，因此 server 掃最近活躍的非 subagent 檔，找「最後一行 `response.toolCalls` 裡有 `consult_advisor` 且 `input.question` 與本次收到的完全一致」的檔——它的 `sessionId` 就是呼叫端。多個 ZCode session 並行不會誤判（不靠 mtime 猜測）。

命中後自動組裝顧問 context：

1. **當前輪獨白**（`response.text`，截 4000 字）置頂——executor 來問之前的想法
2. **完整對話視野**（`request.messages`＝executor 模型實際看到的內容；跳過 system prompt、tool_use 只留名稱與截斷參數、保留尾端 48K 字元）
3. 顧問回覆附 `| session <uuid 前8碼>` 標註歸屬

`question` 參數保留作「聚焦透鏡」——比原版的空輸入多了焦點。

## 安裝

三種方式，擇一。安裝目標 `/usr/local/bin`（ZCode config 不展開 `~`，固定絕對路徑可跨機通用）。

**1. 下載預建 binary**：到 [Releases 最新版](https://github.com/newlix/zcode-advisor/releases/latest)（隨 `v*` tag 自動建置）下載對應資產，並以附帶的 `checksums.txt` 驗證：

| 平台 | 資產檔名（`<ver>` 為該版 tag） |
|---|---|
| Linux x86_64（任何發行版，musl 靜態） | `zcode-advisor-<ver>-x86_64-unknown-linux-musl.tar.gz` |
| Linux aarch64（musl 靜態） | `zcode-advisor-<ver>-aarch64-unknown-linux-musl.tar.gz` |
| macOS Apple Silicon | `zcode-advisor-<ver>-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `zcode-advisor-<ver>-x86_64-apple-darwin.tar.gz` |
| Windows x86_64 | `zcode-advisor-<ver>-x86_64-pc-windows-msvc.zip` |

```bash
tar -xzf zcode-advisor-<ver>-x86_64-unknown-linux-musl.tar.gz
sudo install -m 0755 zcode-advisor /usr/local/bin/zcode-advisor
```

**2. cargo 從 git 安裝**（需 Rust 1.91+）：

```bash
cargo install --git https://github.com/newlix/zcode-advisor
```

**3. 從原始碼建置**：

```bash
git clone https://github.com/newlix/zcode-advisor && cd zcode-advisor
cargo build --release
sudo install -m 0755 target/release/zcode-advisor /usr/local/bin/zcode-advisor
```

（Windows 安裝位置自定，`~\.zcode\cli\config.json` 的 command 記得指到絕對路徑。）

state 檔與日誌落在各 OS 慣例的資料目錄（見「記錄與追查」）。與 Go 版脫鉤：不做舊 state 移轉（計數是短命的節流資料，重置最多多一次提醒），Go 遺留目錄 `~/.zcode/zcode-advisor/` 留給 Go 版原封不動。

在 `~/.zcode/cli/config.json` 註冊（hooks 必須 `hooks.enabled: true`；與 Go 版完全相同的設定）：

```json
{
  "mcp": {
    "servers": {
      "zcode-advisor": {
        "type": "stdio",
        "command": "/usr/local/bin/zcode-advisor",
        "timeoutMs": 120000
      }
    }
  },
  "hooks": {
    "enabled": true,
    "events": {
      "UserPromptSubmit": [
        { "hooks": [{ "type": "command",
            "command": "/usr/local/bin/zcode-advisor hook UserPromptSubmit",
            "timeoutMs": 10000, "statusMessage": "advisor 諮詢提醒" }] }
      ],
      "PostToolUseFailure": [
        { "hooks": [{ "type": "command",
            "command": "/usr/local/bin/zcode-advisor hook PostToolUseFailure",
            "timeoutMs": 120000, "statusMessage": "advisor 卡關診斷…" }] }
      ],
      "PostToolUse": [
        { "hooks": [{ "type": "command",
            "command": "/usr/local/bin/zcode-advisor hook PostToolUseOK",
            "timeoutMs": 10000 }] }
      ]
    }
  }
}
```

重啟 ZCode session 生效。工具名為 `mcp__zcode-advisor__consult_advisor`，參數 `question`（必填）＋ `context`（選填，只補對話裡還沒有的材料）。

建議一併在使用者指示檔（`~/.zcode/AGENTS.md`）加入顧問使用守則——「定位不算實質工作、定下做法前與宣稱完成前各問一次、建議為強先驗、衝突帶回顧問裁決」。

## 顧問模型

唯一後端：本機 Ollama 的 OpenAI 相容端點，模型 `kimi-k3:cloud`（Ollama 的雲端託管模型，走 Ollama 帳號計費，非本地 GGUF——`ollama pull` 拉不下來、`/api/tags` 也看不到它）。**不需要任何 API key**，但需先 `ollama login` 且本機 `ollama serve` 在跑。

**顧問行為的所有設定都在原始碼**：模型／端點／輸出上限 `MAX_TOKENS`（kimi-k3 建議值 131072；reasoning 模型的推理文字計入 max_tokens，預算太小會在推理階段燒光、正文為空）／諮詢上限 `MAX_USES` 在 `src/server.rs`；對話帶入上限 `ROLLOUT_TAIL` 在 `src/rollout.rs`；節流常數（提醒上限 3 次/session、prompt ≥40 字才提醒、卡關門檻連續 2 次失敗、冷卻 5 分鐘、卡關預算 5 次/session）在 `src/hooks.rs`。要調就改常數重編。

執行期不讀任何設定，也沒有環境變數旋鈕——唯一的環境輸入是 ZCode 注入的 `CLAUDE_SESSION_ID`（session 識別）與各平台的標準目錄變數（`HOME`/`USERPROFILE`、`XDG_DATA_HOME`/`LOCALAPPDATA`，由 dirs 解析資料目錄）。

## 檔案

- 執行檔安裝在 `/usr/local/bin/zcode-advisor`；原始碼在本目錄
- `src/server.rs` — rmcp MCP server：`Advisor` handler、`consult_advisor` 工具（spawn_blocking + 串行化）、`ask_advisor`（打本機 Ollama 的 OpenAI 相容端點）、模型常數
- `src/hooks.rs` — 三個 hook 處理器、session 狀態檔（`state/<sess>.state.json`：提醒/失敗/卡關/已諮詢計數）、提醒文字；輸出格式沿用 Go 版 `hooks.go`
- `src/rollout.rs` — UUID 反查、對話壓縮、當前輪獨白抽取；與 Go 版 `rollout.go` 行為一致（fixture 逐位元組對拍驗證）
- `src/http.rs` — 手寫 HTTP/1.1 client（顧問端點是 localhost 純 HTTP；deadline 語義、Content-Length/chunked/close-delimited、1MB body 上限）。rmcp 只提供 MCP transport，對 Ollama 的單發純 HTTP 呼叫不值得再拉 reqwest
- `src/logger.rs` — 行為軌跡日誌（advisor.log）：severity 標記、2MB 輪替保留一代、O_APPEND 單行寫入的多 process 並發安全、寫檔失敗靜默
- `src/util.rs` — 共用工具：char-boundary 安全的 truncate、跨平台資料目錄（dirs）、RFC3339 UTC、私有權限開檔
- `examples/parity.rs` — 對拍工具：輸出 rollout 反查結果，供與 Go 版逐位元組 diff

執行期產物（皆已 gitignore，落點見「記錄與追查」）：`target/`、`state/`、`advisor.log*`、`hooks-debug.log`。

## 記錄與追查

執行期產物集中在單一資料目錄（`util::data_dir()`，跨平台由 [dirs](https://crates.io/crates/dirs) 解析）：

| OS | 位置 |
|---|---|
| Linux | `~/.local/share/zcode-advisor/`（尊重 `$XDG_DATA_HOME`） |
| macOS | `~/Library/Application Support/zcode-advisor/` |
| Windows | `%LOCALAPPDATA%\zcode-advisor\` |

- **`advisor.log`（＋輪替 `.1`）**——行為軌跡：「我們做了什麼、為什麼」。格式 `<RFC3339 UTC> pid=<pid> mode=<server|hook> <LEVEL> <key=value>`，每行一事件：consult 生命週期（question 摘要 → rollout match → `done t=4.2s ctx=48231B advice=1706B`／`failed err=…`）、hook 每個決策點（`decision=remind open=2/3`、`decision=silent reason=consulted|budget|cooldown|short-prompt`…）。追查「顧問為什麼這樣回答」對 `question=`/`sess=` grep；「hook 為什麼沒觸發」看 `decision=silent reason=`。超過 2MB 保留一代後重開（多 process 同時跨過門檻的極窄窗口下為盡力而為，最壞損失舊世代、不損行完整性）；多 process 並發寫靠 O_APPEND＋單行寫入保持完整；寫檔失敗靜默丟棄——記錄系統絕不讓任務失敗。
- **`hooks-debug.log`**——原始擷取：「ZCode 餵了什麼」（每次 hook 的完整 stdin，超過 2MB 重開）。確認 ZCode 實際欄位名用。
- **`state/<sess>.state.json`**——hook 節流計數（提醒/失敗/卡關/已諮詢）。

**等級**：只有 INFO/ERROR 兩種 severity 標記（供 grep 過濾 ERROR），全時全開，**零配置零開關**——行為軌跡本來就該全記；內容重現走 rollout 檔（見下）。檔案為 0600（unix）且在使用者自家目錄。

## 與 Go 版的差異

Hook 輸出（提醒文字、additionalContext 格式）與 rollout 反查逐位元組一致（已實測）；MCP 層改用 rmcp、執行期資料移至跨平台資料目錄後的差異：

- **資料目錄**：state／日誌改落各 OS 慣例位置（見「記錄與追查」），不讀也不寫 Go 遺留的 `~/.zcode/zcode-advisor/`——不做移轉，重置後最多多一次開場提醒。
- **wire 回應形狀**：rmcp 序列化的 key 順序/細節與手寫版不同（spec 合規，client 無感）；`inputSchema` 附帶 `$schema` 欄位、`context` 型別標為 `["string","null"]`（schemars 對 `Option` 的如實描述）。
- **缺參數的錯誤**：缺 `question` 由 rmcp 前置解碼攔下，回 `failed to deserialize parameters: missing field \`question\``（isError 結果，caller 可見）；空白的 `question` 仍回舊版原文。未知方法／參數非物件的錯誤文字也是 rmcp 形狀（`-32601` 只夾帶方法/工具名）；**非 JSON 行靜默丟棄**（Go 回 `-32700` parse error）。
- **版本協商**：支援集合內 echo；不認得的版本回 rmcp 最新（Go 是盲目 echo）。
- **stdin EOF 行為**：rmcp 有 5 秒排水窗——窗內完成的 consult 回應照常送達；超窗的回應不落地，process 在 EOF 後最多約 10 秒退出（`shutdown_timeout(5s)`，不陪著注定丟棄的 HTTP 等到 90s deadline）。MCP client 等待回應期間本就持有管線，實務無影響。
- **HTTP client 不跟隨 redirect、不解 gzip**：端點寫死 localhost，實務不會遇到。
- **卡關 payload 的整數精度**：Go 重編 JSON 時大整數經 float64 會失真，serde_json 原樣保留。
- **並發 hook 寫同一 session 的狀態檔可能交錯**（truncate+write 非原子）：Go 版相同的既有 race；壞了只影響節流計數，下次成功寫入即自癒。MCP 側的 consult 已用 mutex 串行化，無此問題。

## 煙霧測試

```bash
# 單元測試（工具註冊/schema、Ollama 回應解碼、HTTP 假 server、截斷邊界、狀態機）
cargo test

# 協議（initialize → notification → tools/list；等回應期間管線保持開啟）
( printf '%s\n%s\n%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'; sleep 2 ) \
  | ./target/release/zcode-advisor

# 端到端（會真的打 API；reasoning 模型的思考段計入 max_tokens，
# 太小會在推理階段燒光而回空內容——此時錯誤訊息會指名源碼裡的 MAX_TOKENS 常數）
( printf '%s\n%s\n%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"consult_advisor","arguments":{"question":"回覆OK"}}}'; sleep 30 ) \
  | ./target/release/zcode-advisor

# hook（不打 API；輸出與 Go版 byte-identical）
echo '{"prompt":"…40字以上的任務描述…","session_id":"s1"}' | ./target/release/zcode-advisor hook UserPromptSubmit
```

**內容重現**：advisor.log 只記結構軌跡（決策、結果、時間、大小），不記內容——「顧問到底看到什麼」（完整 question、對話視野、建議本文）由 ZCode 的 rollout 檔原生保存且永久可查：

```bash
# 找出某次 consult 的完整提問與回覆（question 在 toolCall input、建議在下一輪的 tool result）
grep -l consult_advisor ~/.zcode/cli/rollout/model-io-sess_*.jsonl | tail -5
```

疑難排解：hook 沒觸發 → 先確認 `hooks.enabled: true`，再看 `hooks-debug.log` 有無記錄（有 config 問題參照 ZCode 的 diagnosing-hooks 指南）；MCP 連不上 → Settings → MCP 看狀態，config-file server 的 schema 是嚴格的（未知欄位會整個被丟棄）、路徑必須絕對。
