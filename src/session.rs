//! 会话状态管理模块。
//!
//! 管理 Agent 与模型之间的对话上下文：
//! - 累积对话历史 item
//! - 将历史 + system prompt + 工具定义拼装为 `ModelRequest`
//! - 记录 token 消耗并在超预算时截断旧 item

use crate::model::{Item, Message, ModelRequest, TokenUsage, ToolSpec};

// ==================== 常量 ====================

/// Token 估算：每 4 字节约等于 1 个 token。
///
/// 参考 Codex CLI `context_manager/history.rs` 的 `approx_token_count()`。
const BYTES_PER_TOKEN: usize = 4;

// ==================== Session ====================

/// 无状态会话管理器。
///
/// 持有 system prompt 和对话历史 item，负责拼装 `ModelRequest`。
/// 每次调用 `build_request` 发送全部历史（无状态），
/// 不维护服务端 session。
///
/// # 数据流
///
/// ```text
/// 用户输入 → push(User) → build_request(tools) → model.stream()
///                                                    ↓
///                              TextDone / ToolCallReady / Completed
///                                                    ↓
///                               push(Assistant/ToolCall) + record_usage()
///                                                    ↓
///                                            有 tool_calls?
///                                           /            \
///                                         是              否
///                                push(ToolResult)      turn 结束
///                                         ↓
///                                   build_request()  ← 循环
/// ```
pub struct Session {
    /// 固定的 system prompt，每次请求作为第一条 `Item::Message(Message::System)` 发送。
    system_prompt: String,
    /// 对话历史（不含 system prompt）。
    items: Vec<Item>,
    /// 累计消耗的 token 总数（从 API `usage` 字段获取）。
    total_tokens: u32,
    /// Token 预算上限。超过时 `truncate_to_budget()` 会删除最旧的 item。
    ///
    /// 参考 Claude Code `autoCompact.ts`:
    /// `threshold = contextWindow - AUTOCOMPACT_BUFFER_TOKENS(13000)`
    max_context_tokens: u32,
}

impl Session {
    /// 创建一个新会话。
    ///
    /// `max_context_tokens` 通常设为 `模型窗口 - 缓冲(如 13000)`。
    /// Phase 1 由调用方自行计算；将来 `config.rs` 可根据 model 自动推导。
    pub fn new(system_prompt: impl Into<String>, max_context_tokens: u32) -> Self {
        Self {
            system_prompt: system_prompt.into(),
            items: Vec::new(),
            total_tokens: 0,
            max_context_tokens,
        }
    }

    /// 追加一个 item 到对话历史。
    ///
    /// Agent 循环中每收到一个 item 就调用：User 输入、Assistant 回复、ToolCall、ToolResult。
    /// 参考 Codex 风格：history 以统一 item 序列累积。
    pub fn push(&mut self, item: Item) {
        self.items.push(item);
    }

    /// 返回对话历史的只读切片。
    pub fn items(&self) -> &[Item] {
        &self.items
    }

    /// 返回对话历史中的 item 数量。
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// 返回对话历史是否为空。
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 拼装完整的 `ModelRequest`，供 `Model::stream()` / `Model::send()` 使用。
    ///
    /// 组装顺序：`[Item::Message(System), ...self.items, tools]`
    ///
    /// 参考 Claude Code `claude.ts` 的请求拼装：
    /// system prompt 独立于 messages，通过请求体顶层 `system` 字段发送。
    /// funcode 中统一为 `Item::Message(Message::System)` 放在 items[0]，
    /// `model.rs` 的 provider adapter 会将其转为对应 provider 的格式。
    pub fn build_request(&self, tools: &[ToolSpec]) -> ModelRequest {
        let mut all = Vec::with_capacity(self.items.len() + 1);
        all.push(Item::message(Message::system(&self.system_prompt)));
        all.extend(self.items.iter().cloned());

        ModelRequest {
            items: all,
            tools: tools.to_vec(),
            temperature: None,
        }
    }

    /// 从 API 响应的 `usage` 字段记录 token 消耗。
    ///
    /// 参考 Claude Code `autoCompact.ts`: token 数来自 API response 的
    /// `usage.input_tokens + usage.cache_read_input_tokens`。
    /// Phase 1 简化为直接使用 `total_tokens`。
    pub fn record_usage(&mut self, usage: TokenUsage) {
        if let Some(total) = usage.total_tokens {
            self.total_tokens = self.total_tokens.saturating_add(total);
        } else {
            let sum = usage.input_tokens.unwrap_or(0) + usage.output_tokens.unwrap_or(0);
            self.total_tokens = self.total_tokens.saturating_add(sum);
        }
    }

    /// 返回当前累计消耗的 token 总数。
    pub fn total_tokens(&self) -> u32 {
        self.total_tokens
    }

    /// 按 token 预算截断旧 item。
    ///
    /// 使用 `len() / 4` 启发式估算每个 item 的 token 数（参考 Codex CLI
    /// `approx_token_count`），从最旧的 item 开始删除，直到总量低于预算。
    ///
    /// 保留最近一条 User item 不截断，确保 agent 始终知道用户在问什么。
    /// system prompt 不计入预算（它始终在请求头部，由 provider 管理）。
    ///
    /// 参考 Claude Code 的 `snipCompact`（兜底截断策略）。
    pub fn truncate_to_budget(&mut self) {
        let budget = self.max_context_tokens as usize;

        // 找到最近一条 User item 的索引，保证不被截断
        let last_user_idx = self
            .items
            .iter()
            .rposition(|item| matches!(item, Item::Message(Message::User(_))));

        while self.estimate_tokens() > budget && self.items.len() > 1 {
            // 如果第一条就是要保留的 User item，停止截断
            if last_user_idx == Some(0) {
                break;
            }
            self.items.remove(0);
        }
    }

    /// 清空对话历史和 token 计数，保留 system prompt。
    ///
    /// 用于新会话或 compaction 后重建。
    /// 参考 Claude Code: `state.messages = compactedMessages`。
    pub fn clear(&mut self) {
        self.items.clear();
        self.total_tokens = 0;
    }

    // ==================== 私有方法 ====================

    /// 估算当前对话历史的 token 总数。
    ///
    /// 使用 `text_content().len() / 4` 启发式，参考 Codex CLI:
    /// ```ignore
    /// const APPROX_BYTES_PER_TOKEN: usize = 4;
    /// pub fn approx_token_count(text: &str) -> usize {
    ///     (text.len() + 3) / 4
    /// }
    /// ```
    fn estimate_tokens(&self) -> usize {
        self.items
            .iter()
            .map(|item| {
                let text = item.text_content().map(|t| t.len()).unwrap_or(0);
                (text + BYTES_PER_TOKEN - 1) / BYTES_PER_TOKEN
            })
            .sum()
    }
}

// ==================== Tests ====================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ToolCall, ToolResult};
    use serde_json::json;

    // === 构造与基本访问 ===

    #[test]
    fn new_session_is_empty() {
        let session = Session::new("You are helpful.", 100_000);
        assert!(session.is_empty());
        assert_eq!(session.len(), 0);
        assert!(session.items().is_empty());
        assert_eq!(session.total_tokens(), 0);
    }

    #[test]
    fn push_and_read_items() {
        let mut session = Session::new("You are helpful.", 100_000);
        session.push(Item::user("hello"));
        session.push(Item::assistant("hi"));

        assert_eq!(session.len(), 2);
        assert!(matches!(
            session.items()[0],
            Item::Message(Message::User(ref s)) if s == "hello"
        ));
        assert!(matches!(
            session.items()[1],
            Item::Message(Message::Assistant(ref s)) if s == "hi"
        ));
    }

    // === build_request ===

    #[test]
    fn build_request_includes_system_prompt() {
        let session = Session::new("You are a Rust expert.", 100_000);

        let req = session.build_request(&[]);

        assert_eq!(req.items.len(), 1);
        assert!(matches!(
            &req.items[0],
            Item::Message(Message::System(s)) if s == "You are a Rust expert."
        ));
    }

    #[test]
    fn build_request_prepends_system_to_history() {
        let mut session = Session::new("system", 100_000);
        session.push(Item::user("hello"));
        session.push(Item::assistant("hi"));

        let req = session.build_request(&[]);

        assert_eq!(req.items.len(), 3);
        assert!(matches!(&req.items[0], Item::Message(Message::System(_))));
        assert!(matches!(&req.items[1], Item::Message(Message::User(_))));
        assert!(matches!(
            &req.items[2],
            Item::Message(Message::Assistant(_))
        ));
    }

    #[test]
    fn build_request_includes_tools() {
        let session = Session::new("system", 100_000);
        let tools = vec![ToolSpec::new(
            "read_file",
            "Read a file",
            json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        )];

        let req = session.build_request(&tools);

        assert_eq!(req.tools.len(), 1);
        assert_eq!(req.tools[0].name, "read_file");
    }

    #[test]
    fn build_request_empty_tools() {
        let session = Session::new("system", 100_000);
        let req = session.build_request(&[]);
        assert!(req.tools.is_empty());
    }

    #[test]
    fn build_request_does_not_mutate_session() {
        let mut session = Session::new("system", 100_000);
        session.push(Item::user("hello"));

        let _ = session.build_request(&[]);
        let _ = session.build_request(&[]);

        // build_request 不应改变 items
        assert_eq!(session.len(), 1);
        // items 里只有 User，不含 System（System 在 build_request 时拼装）
        assert!(matches!(
            session.items()[0],
            Item::Message(Message::User(_))
        ));
    }

    // === record_usage ===

    #[test]
    fn record_usage_with_total() {
        let mut session = Session::new("system", 100_000);
        session.record_usage(TokenUsage {
            input_tokens: Some(100),
            output_tokens: Some(50),
            total_tokens: Some(150),
        });
        assert_eq!(session.total_tokens(), 150);

        session.record_usage(TokenUsage {
            input_tokens: Some(200),
            output_tokens: Some(80),
            total_tokens: Some(280),
        });
        assert_eq!(session.total_tokens(), 430);
    }

    #[test]
    fn record_usage_without_total_sums_components() {
        let mut session = Session::new("system", 100_000);
        session.record_usage(TokenUsage {
            input_tokens: Some(100),
            output_tokens: Some(50),
            total_tokens: None,
        });
        assert_eq!(session.total_tokens(), 150);
    }

    #[test]
    fn record_usage_saturates() {
        let mut session = Session::new("system", 100_000);
        session.total_tokens = u32::MAX;
        session.record_usage(TokenUsage {
            input_tokens: Some(1),
            output_tokens: None,
            total_tokens: None,
        });
        assert_eq!(session.total_tokens(), u32::MAX);
    }

    // === truncate_to_budget ===

    #[test]
    fn truncate_removes_oldest_items() {
        let mut session = Session::new("system", 10); // 极小预算
        // 每个 item 约 3 token ("aaa" = 3 bytes / 4 ≈ 1 token)
        // 塞入 20 条，远超预算
        for i in 0..20 {
            session.push(Item::user(format!("aaa{i}")));
        }

        session.truncate_to_budget();

        assert!(session.estimate_tokens() <= 10);
    }

    #[test]
    fn truncate_keeps_last_user_item() {
        let mut session = Session::new("system", 10);
        for i in 0..20 {
            session.push(Item::user(format!("msg{i}")));
        }

        let last_text = session
            .items()
            .last()
            .and_then(|item| item.text_content())
            .unwrap()
            .to_string();
        session.truncate_to_budget();

        // 最后一条 User item 必须保留
        let final_text = session
            .items()
            .last()
            .and_then(|item| item.text_content())
            .unwrap();
        assert_eq!(final_text, last_text);
    }

    #[test]
    fn truncate_noop_when_within_budget() {
        let mut session = Session::new("system", 1_000_000);
        session.push(Item::user("short"));
        session.push(Item::assistant("ok"));

        let len_before = session.len();
        session.truncate_to_budget();
        assert_eq!(session.len(), len_before);
    }

    // === clear ===

    #[test]
    fn clear_resets_history_but_keeps_prompt() {
        let mut session = Session::new("system prompt", 100_000);
        session.push(Item::user("hello"));
        session.record_usage(TokenUsage {
            input_tokens: Some(100),
            output_tokens: Some(50),
            total_tokens: Some(150),
        });

        session.clear();

        assert!(session.is_empty());
        assert_eq!(session.total_tokens(), 0);

        // build_request 仍然包含 system prompt
        let req = session.build_request(&[]);
        assert_eq!(req.items.len(), 1);
        assert!(matches!(
            &req.items[0],
            Item::Message(Message::System(s)) if s == "system prompt"
        ));
    }

    // === 完整 Agent 循环模拟 ===

    #[test]
    fn simulate_agent_turn_with_tool_call() {
        let mut session = Session::new("You are helpful.", 100_000);
        let tools = vec![ToolSpec::new(
            "read_file",
            "Read a file",
            json!({"type": "object"}),
        )];

        // 1. 用户输入
        session.push(Item::user("Read src/main.rs"));

        // 2. 模型返回 assistant + tool_call
        let req1 = session.build_request(&tools);
        assert_eq!(req1.items[0].text_content(), Some("You are helpful."));
        assert_eq!(req1.items[1].text_content(), Some("Read src/main.rs"));

        session.push(Item::assistant("Let me read that file."));
        session.push(Item::tool_call(ToolCall::new(
            "call_1",
            "read_file",
            r#"{"path":"src/main.rs"}"#,
        )));
        session.record_usage(TokenUsage {
            input_tokens: Some(500),
            output_tokens: Some(100),
            total_tokens: Some(600),
        });

        // 3. 工具执行结果
        let result = ToolResult::new("call_1", "read_file", "fn main() {}");
        session.push(Item::tool_result(result));

        // 4. 构建下一轮请求（此时还没有 push 最终回复）
        let req2 = session.build_request(&tools);
        assert_eq!(req2.items.len(), 5); // system + user + assistant + tool_call + tool_result

        session.push(Item::assistant("The file contains `fn main() {}`."));
        session.record_usage(TokenUsage {
            input_tokens: Some(800),
            output_tokens: Some(200),
            total_tokens: Some(1000),
        });

        // 验证最终状态
        assert_eq!(session.len(), 5); // user + assistant + tool_call + tool_result + assistant
        assert_eq!(session.total_tokens(), 1600);
    }
}
