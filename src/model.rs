//! 模型接入模块。
//!
//! 这个文件定义内部统一消息模型，并通过 `async-openai` 库
//! 实现 OpenAI `chat/completions` 接口对接。

#![allow(dead_code)]

use std::pin::Pin;
use std::task::{Context, Poll};

use async_openai::config::OpenAIConfig;
use async_openai::error::OpenAIError;
use async_openai::types::chat::{
    ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls,
    ChatCompletionRequestAssistantMessage, ChatCompletionRequestAssistantMessageContent,
    ChatCompletionRequestMessage, ChatCompletionRequestSystemMessage,
    ChatCompletionRequestSystemMessageContent, ChatCompletionRequestToolMessage,
    ChatCompletionRequestToolMessageContent, ChatCompletionRequestUserMessage,
    ChatCompletionRequestUserMessageContent, ChatCompletionResponseMessage,
    ChatCompletionStreamResponseDelta, ChatCompletionTool, ChatCompletionTools,
    CompletionUsage, CreateChatCompletionRequest,
    CreateChatCompletionResponse, CreateChatCompletionStreamResponse, FinishReason, FunctionCall,
    FunctionObject,
};
use async_openai::Client as OpenAIClient;
use futures_util::{Stream, StreamExt};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::mpsc;

// ==================== 公共消息类型 ====================

/// 模型层使用的统一消息结构。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    pub role: Role,
    pub parts: Vec<Part>,
    pub name: Option<String>,
}

impl ChatMessage {
    /// 创建一个自定义角色消息。
    pub fn new(role: Role, parts: Vec<Part>) -> Self {
        Self {
            role,
            parts,
            name: None,
        }
    }

    /// 创建仅包含文本内容的消息。
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self::new(role, vec![Part::text(text)])
    }

    pub fn system(text: impl Into<String>) -> Self {
        Self::text(Role::System, text)
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self::text(Role::User, text)
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self::text(Role::Assistant, text)
    }

    pub fn tool_result(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self::new(
            Role::Tool,
            vec![Part::tool_result(tool_call_id, tool_name, content)],
        )
    }

    /// 给消息附加逻辑名称，例如 few-shot 示例中的 `name` 字段。
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// 为消息追加一个内容片段。
    pub fn push_part(&mut self, part: Part) {
        self.parts.push(part);
    }

    /// 便捷追加一个文本片段。
    pub fn push_text(&mut self, text: impl Into<String>) {
        self.push_part(Part::text(text));
    }

    /// 判断消息是否不包含任何内容片段。
    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    /// 提取消息中的所有纯文本片段，常用于日志或 provider 降级兼容。
    pub fn text_parts(&self) -> impl Iterator<Item = &str> {
        self.parts.iter().filter_map(Part::as_text)
    }
}

/// 消息角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// 单条消息中的内容片段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part {
    Text(Text),
    ToolCall(ToolCall),
    ToolResult(ToolResult),
}

impl Part {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(Text::new(text))
    }

    pub fn tool_call(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        Self::ToolCall(ToolCall::new(id, name, arguments))
    }

    pub fn tool_result(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self::ToolResult(ToolResult::new(tool_call_id, tool_name, content))
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(part) => Some(part.text.as_str()),
            _ => None,
        }
    }
}

/// 文本片段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Text {
    pub text: String,
}

impl Text {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

/// 模型发起的一次工具调用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// 保留为原始字符串，便于后续接入 JSON 或 provider 特定参数编码。
    pub arguments: String,
}

impl ToolCall {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }
}

/// 工具执行结果，通常会作为一条 `tool` 角色消息回传给模型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: String,
    pub is_error: bool,
}

impl ToolResult {
    pub fn new(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            content: content.into(),
            is_error: false,
        }
    }

    pub fn error(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            content: content.into(),
            is_error: true,
        }
    }
}

// ==================== 请求 / 响应 / 流式类型 ====================

/// 一次模型调用请求。
#[derive(Debug, Clone, PartialEq)]
pub struct ModelRequest {
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolSpec>,
    pub temperature: Option<f32>,
}

impl ModelRequest {
    pub fn new(messages: Vec<ChatMessage>) -> Self {
        Self {
            messages,
            tools: Vec::new(),
            temperature: None,
        }
    }

    pub fn with_tools(mut self, tools: Vec<ToolSpec>) -> Self {
        self.tools = tools;
        self
    }

    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = Some(temperature);
        self
    }
}

/// 一次模型调用响应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelResponse {
    pub message: ChatMessage,
    pub finish_reason: Option<String>,
    pub usage: Option<TokenUsage>,
}

/// 流式响应事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseEvent {
    TextDelta(String),
    ToolCallStart {
        id: String,
        name: String,
    },
    ToolCallArgumentsDelta {
        id: String,
        chunk: String,
    },
    ToolCallDone {
        id: String,
        name: String,
        arguments: String,
    },
    MessageDone(ModelResponse),
}

/// 模型流式响应对象。
pub struct ResponseStream {
    pub rx_event: mpsc::Receiver<Result<ResponseEvent, ModelError>>,
}

impl ResponseStream {
    fn new(rx_event: mpsc::Receiver<Result<ResponseEvent, ModelError>>) -> Self {
        Self { rx_event }
    }
}

impl Stream for ResponseStream {
    type Item = Result<ResponseEvent, ModelError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx_event.poll_recv(cx)
    }
}

/// 暴露给上层的工具定义。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

impl ToolSpec {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
        }
    }
}

/// token 使用统计。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub total_tokens: Option<u32>,
}

// ==================== 错误类型 ====================

/// 模型调用错误。
#[derive(Debug, Error)]
pub enum ModelError {
    #[error("model name cannot be empty")]
    EmptyModelName,
    #[error("OpenAI API key cannot be empty")]
    EmptyApiKey,
    #[error("invalid message shape: {0}")]
    InvalidMessage(&'static str),
    #[error("model returned no choices")]
    EmptyChoices,
    #[error("stream protocol error: {0}")]
    StreamProtocol(&'static str),
    #[error("OpenAI error: {0}")]
    OpenAI(#[from] OpenAIError),
}

// ==================== Model：基于 async-openai 的统一模型对象 ====================

/// 统一模型对象，内部使用 `async-openai` 库实现 OpenAI `chat/completions` 调用。
#[derive(Debug, Clone)]
pub struct Model {
    client: OpenAIClient<OpenAIConfig>,
    model: String,
}

impl Model {
    pub fn new(
        api_key: impl Into<String>,
        model: impl Into<String>,
        base_url: Option<String>,
        _timeout_secs: Option<u64>,
    ) -> Result<Self, ModelError> {
        let api_key = api_key.into();
        if api_key.trim().is_empty() {
            return Err(ModelError::EmptyApiKey);
        }

        let model = model.into();
        if model.trim().is_empty() {
            return Err(ModelError::EmptyModelName);
        }

        let mut config = OpenAIConfig::new().with_api_key(api_key);
        if let Some(base_url) = base_url {
            let trimmed = base_url.trim().to_string();
            if !trimmed.is_empty() {
                config = config.with_api_base(trimmed);
            }
        }

        let client = OpenAIClient::with_config(config);
        Ok(Self { client, model })
    }

    pub async fn send(&self, request: ModelRequest) -> Result<ModelResponse, ModelError> {
        let req = build_request(&self.model, request)?;
        let response = self.client.chat().create(req).await?;
        parse_response(response)
    }

    pub async fn stream(&self, request: ModelRequest) -> Result<ResponseStream, ModelError> {
        let req = build_request(&self.model, request)?;
        let stream = self.client.chat().create_stream(req).await?;

        let (tx_event, rx_event) = mpsc::channel(32);
        tokio::spawn(async move {
            let mut accumulator = StreamAccumulator::default();
            let stream: async_openai::types::chat::ChatCompletionResponseStream = stream;
            let mut stream = std::pin::pin!(stream);

            while let Some(result) = stream.next().await {
                let chunk = match result {
                    Ok(chunk) => chunk,
                    Err(err) => {
                        let _ = tx_event.send(Err(ModelError::OpenAI(err))).await;
                        return;
                    }
                };

                let events = match accumulator.apply_chunk(&chunk) {
                    Ok(events) => events,
                    Err(err) => {
                        let _ = tx_event.send(Err(err)).await;
                        return;
                    }
                };

                for event in events {
                    if tx_event.send(Ok(event)).await.is_err() {
                        return;
                    }
                }
            }

            // 流结束后 flush accumulator
            let events = match accumulator.finish() {
                Ok(events) => events,
                Err(err) => {
                    let _ = tx_event.send(Err(err)).await;
                    return;
                }
            };
            for event in events {
                if tx_event.send(Ok(event)).await.is_err() {
                    return;
                }
            }
        });

        Ok(ResponseStream::new(rx_event))
    }
}

// ==================== 请求构建（内部转换） ====================

fn build_request(
    model: &str,
    request: ModelRequest,
) -> Result<CreateChatCompletionRequest, ModelError> {
    let messages = request
        .messages
        .iter()
        .map(chat_message_to_openai)
        .collect::<Result<Vec<_>, _>>()?;

    let mut req = CreateChatCompletionRequest {
        model: model.to_string(),
        messages,
        tools: None,
        temperature: request.temperature,
        stream: None,
        ..Default::default()
    };

    if !request.tools.is_empty() {
        req.tools = Some(
            request
                .tools
                .into_iter()
                .map(|tool| {
                    ChatCompletionTools::Function(ChatCompletionTool {
                        function: FunctionObject {
                            name: tool.name,
                            description: Some(tool.description),
                            parameters: Some(tool.input_schema),
                            strict: None,
                        },
                    })
                })
                .collect(),
        );
    }

    Ok(req)
}

fn chat_message_to_openai(message: &ChatMessage) -> Result<ChatCompletionRequestMessage, ModelError> {
    match message.role {
        Role::System | Role::User => {
            let content = collect_text_content(message)?;
            match message.role {
                Role::System => Ok(ChatCompletionRequestMessage::System(
                    ChatCompletionRequestSystemMessage {
                        content: ChatCompletionRequestSystemMessageContent::Text(content),
                        name: message.name.clone(),
                    },
                )),
                Role::User => Ok(ChatCompletionRequestMessage::User(
                    ChatCompletionRequestUserMessage {
                        content: ChatCompletionRequestUserMessageContent::Text(content),
                        name: message.name.clone(),
                    },
                )),
                _ => unreachable!(),
            }
        }
        Role::Assistant => {
            let mut text_parts = Vec::new();
            let mut tool_calls = Vec::new();

            for part in &message.parts {
                match part {
                    Part::Text(text) => text_parts.push(text.text.clone()),
                    Part::ToolCall(call) => {
                        tool_calls.push(ChatCompletionMessageToolCalls::Function(
                            ChatCompletionMessageToolCall {
                                id: call.id.clone(),
                                function: FunctionCall {
                                    name: call.name.clone(),
                                    arguments: call.arguments.clone(),
                                },
                            },
                        ));
                    }
                    Part::ToolResult(_) => {
                        return Err(ModelError::InvalidMessage(
                            "assistant messages cannot contain tool results",
                        ));
                    }
                }
            }

            Ok(ChatCompletionRequestMessage::Assistant(
                ChatCompletionRequestAssistantMessage {
                    content: join_text_parts(text_parts)
                        .map(ChatCompletionRequestAssistantMessageContent::Text),
                    name: message.name.clone(),
                    tool_calls: if tool_calls.is_empty() {
                        None
                    } else {
                        Some(tool_calls)
                    },
                    ..Default::default()
                },
            ))
        }
        Role::Tool => {
            if message.parts.len() != 1 {
                return Err(ModelError::InvalidMessage(
                    "tool messages must contain exactly one tool result",
                ));
            }

            let Part::ToolResult(result) = &message.parts[0] else {
                return Err(ModelError::InvalidMessage(
                    "tool messages must contain a tool result part",
                ));
            };

            Ok(ChatCompletionRequestMessage::Tool(
                ChatCompletionRequestToolMessage {
                    content: ChatCompletionRequestToolMessageContent::Text(result.content.clone()),
                    tool_call_id: result.tool_call_id.clone(),
                },
            ))
        }
    }
}

fn collect_text_content(message: &ChatMessage) -> Result<String, ModelError> {
    let mut text_parts = Vec::new();

    for part in &message.parts {
        match part {
            Part::Text(text) => text_parts.push(text.text.clone()),
            Part::ToolCall(_) | Part::ToolResult(_) => {
                return Err(ModelError::InvalidMessage(
                    "system/user messages can only contain text parts",
                ));
            }
        }
    }

    Ok(text_parts.join("\n\n"))
}

fn join_text_parts(parts: Vec<String>) -> Option<String> {
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

// ==================== 响应解析（内部转换） ====================

fn parse_response(response: CreateChatCompletionResponse) -> Result<ModelResponse, ModelError> {
    let choice = response
        .choices
        .into_iter()
        .next()
        .ok_or(ModelError::EmptyChoices)?;
    let message = openai_message_to_chat(choice.message)?;
    let usage = response.usage.map(token_usage_from_completion);

    Ok(ModelResponse {
        message,
        finish_reason: choice.finish_reason.map(finish_reason_to_string),
        usage,
    })
}

fn openai_message_to_chat(
    message: ChatCompletionResponseMessage,
) -> Result<ChatMessage, ModelError> {
    let role = convert_role(&message.role)?;
    let mut parts = Vec::new();

    if let Some(content) = message.content {
        if !content.is_empty() {
            parts.push(Part::text(content));
        }
    }

    if let Some(tool_calls) = message.tool_calls {
        for tool_call in tool_calls {
            match tool_call {
                ChatCompletionMessageToolCalls::Function(func_call) => {
                    parts.push(Part::tool_call(
                        func_call.id,
                        func_call.function.name,
                        func_call.function.arguments,
                    ));
                }
                ChatCompletionMessageToolCalls::Custom(_) => {
                    return Err(ModelError::InvalidMessage(
                        "custom tool calls are not supported",
                    ));
                }
            }
        }
    }

    Ok(ChatMessage {
        role,
        parts,
        name: None,
    })
}

fn convert_role(role: &async_openai::types::chat::Role) -> Result<Role, ModelError> {
    match role {
        async_openai::types::chat::Role::System => Ok(Role::System),
        async_openai::types::chat::Role::User => Ok(Role::User),
        async_openai::types::chat::Role::Assistant => Ok(Role::Assistant),
        async_openai::types::chat::Role::Tool => Ok(Role::Tool),
        _ => Err(ModelError::InvalidMessage("unsupported response role")),
    }
}

fn finish_reason_to_string(reason: FinishReason) -> String {
    match reason {
        FinishReason::Stop => "stop".to_string(),
        FinishReason::Length => "length".to_string(),
        FinishReason::ToolCalls => "tool_calls".to_string(),
        FinishReason::ContentFilter => "content_filter".to_string(),
        FinishReason::FunctionCall => "function_call".to_string(),
    }
}

fn token_usage_from_completion(usage: CompletionUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: Some(usage.prompt_tokens),
        output_tokens: Some(usage.completion_tokens),
        total_tokens: Some(usage.total_tokens),
    }
}

// ==================== 流式累加器 ====================

#[derive(Debug, Default)]
struct StreamAccumulator {
    role: Option<Role>,
    text: String,
    tool_calls: Vec<PartialToolCall>,
    finish_reason: Option<String>,
    usage: Option<TokenUsage>,
}

#[derive(Debug, Default)]
struct PartialToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
    announced_start: bool,
    pending_argument_chunks: Vec<String>,
}

impl StreamAccumulator {
    fn apply_chunk(
        &mut self,
        chunk: &CreateChatCompletionStreamResponse,
    ) -> Result<Vec<ResponseEvent>, ModelError> {
        let mut events = Vec::new();

        if let Some(usage) = &chunk.usage {
            self.usage = Some(token_usage_from_completion(usage.clone()));
        }

        for choice in &chunk.choices {
            if choice.index != 0 {
                continue;
            }
            self.apply_delta(&choice.delta, &choice.finish_reason, &mut events)?;
        }

        Ok(events)
    }

    fn apply_delta(
        &mut self,
        delta: &ChatCompletionStreamResponseDelta,
        finish_reason: &Option<FinishReason>,
        events: &mut Vec<ResponseEvent>,
    ) -> Result<(), ModelError> {
        if let Some(role) = &delta.role {
            self.role = Some(convert_role(role)?);
        }

        if let Some(content) = &delta.content {
            if !content.is_empty() {
                self.text.push_str(content);
                events.push(ResponseEvent::TextDelta(content.clone()));
            }
        }

        if let Some(tool_calls) = &delta.tool_calls {
            for tc_delta in tool_calls {
                let partial =
                    ensure_partial_tool_call(&mut self.tool_calls, tc_delta.index as usize)?;
                apply_tool_call_delta(partial, tc_delta, events)?;
            }
        }

        if let Some(reason) = finish_reason {
            self.finish_reason = Some(finish_reason_to_string(*reason));
        }

        Ok(())
    }

    fn finish(self) -> Result<Vec<ResponseEvent>, ModelError> {
        let mut events = Vec::new();
        let role = self.role.unwrap_or(Role::Assistant);
        let mut parts = Vec::new();

        if !self.text.is_empty() {
            parts.push(Part::text(self.text.clone()));
        }

        for call in self.tool_calls {
            let id = call
                .id
                .ok_or(ModelError::StreamProtocol("tool call id missing at stream end"))?;
            let name = call
                .name
                .ok_or(ModelError::StreamProtocol(
                    "tool call name missing at stream end",
                ))?;

            events.push(ResponseEvent::ToolCallDone {
                id: id.clone(),
                name: name.clone(),
                arguments: call.arguments.clone(),
            });
            parts.push(Part::tool_call(id, name, call.arguments));
        }

        events.push(ResponseEvent::MessageDone(ModelResponse {
            message: ChatMessage {
                role,
                parts,
                name: None,
            },
            finish_reason: self.finish_reason,
            usage: self.usage,
        }));

        Ok(events)
    }
}

fn apply_tool_call_delta(
    partial: &mut PartialToolCall,
    delta: &async_openai::types::chat::ChatCompletionMessageToolCallChunk,
    events: &mut Vec<ResponseEvent>,
) -> Result<(), ModelError> {
    if let Some(id) = &delta.id {
        partial.id = Some(id.clone());
    }

    if let Some(function) = &delta.function {
        if let Some(name) = &function.name {
            partial.name = Some(name.clone());
        }

        if let Some(arguments) = &function.arguments {
            partial.arguments.push_str(arguments);
            if partial.announced_start {
                if let Some(id) = partial.id.clone() {
                    events.push(ResponseEvent::ToolCallArgumentsDelta {
                        id,
                        chunk: arguments.to_string(),
                    });
                }
            } else {
                partial.pending_argument_chunks.push(arguments.to_string());
            }
        }
    }

    maybe_emit_tool_call_start(partial, events);
    Ok(())
}

fn maybe_emit_tool_call_start(partial: &mut PartialToolCall, events: &mut Vec<ResponseEvent>) {
    if partial.announced_start {
        return;
    }

    let (Some(id), Some(name)) = (partial.id.clone(), partial.name.clone()) else {
        return;
    };

    partial.announced_start = true;
    events.push(ResponseEvent::ToolCallStart {
        id: id.clone(),
        name,
    });

    for chunk in partial.pending_argument_chunks.drain(..) {
        events.push(ResponseEvent::ToolCallArgumentsDelta {
            id: id.clone(),
            chunk,
        });
    }
}

fn ensure_partial_tool_call(
    tool_calls: &mut Vec<PartialToolCall>,
    index: usize,
) -> Result<&mut PartialToolCall, ModelError> {
    if index > tool_calls.len() {
        return Err(ModelError::StreamProtocol(
            "tool call delta index skipped unexpectedly",
        ));
    }

    if index == tool_calls.len() {
        tool_calls.push(PartialToolCall::default());
    }

    tool_calls
        .get_mut(index)
        .ok_or(ModelError::StreamProtocol(
            "tool call delta index out of bounds",
        ))
}

// ==================== 测试 ====================

#[cfg(test)]
mod tests {
    use super::*;
    use async_openai::types::chat::{
        ChatChoice, ChatChoiceStream, ChatCompletionMessageToolCallChunk, FunctionCallStream,
    };
    use futures_util::StreamExt;
    use serde_json::json;

    #[test]
    fn create_text_messages() {
        let system = ChatMessage::system("You are helpful.");
        let user = ChatMessage::user("hello");
        let assistant = ChatMessage::assistant("hi");

        assert_eq!(system.role, Role::System);
        assert_eq!(user.role, Role::User);
        assert_eq!(assistant.role, Role::Assistant);
        assert_eq!(user.text_parts().collect::<Vec<_>>(), vec!["hello"]);
    }

    #[test]
    fn collect_multiple_text_parts() {
        let mut message = ChatMessage::new(Role::Assistant, Vec::new());
        message.push_text("first");
        message.push_part(Part::tool_call("call_1", "read_file", r#"{"path":"src/main.rs"}"#));
        message.push_text("second");

        assert_eq!(
            message.text_parts().collect::<Vec<_>>(),
            vec!["first", "second"]
        );
    }

    #[test]
    fn create_tool_result_message() {
        let message = ChatMessage::tool_result("call_1", "read_file", "fn main() {}");

        assert_eq!(message.role, Role::Tool);
        assert_eq!(message.parts.len(), 1);

        match &message.parts[0] {
            Part::ToolResult(result) => {
                assert_eq!(result.tool_call_id, "call_1");
                assert_eq!(result.tool_name, "read_file");
                assert!(!result.is_error);
            }
            _ => panic!("expected tool result"),
        }
    }

    #[test]
    fn build_request_with_tool_calls() {
        let request = ModelRequest::new(vec![
            ChatMessage::system("You are helpful."),
            ChatMessage::user("hello"),
            ChatMessage {
                role: Role::Assistant,
                parts: vec![
                    Part::text("let me check"),
                    Part::tool_call("call_1", "read_file", r#"{"path":"src/main.rs"}"#),
                ],
                name: None,
            },
            ChatMessage::tool_result("call_1", "read_file", "file content"),
        ])
        .with_tools(vec![ToolSpec::new(
            "read_file",
            "Read a file from the workspace",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                },
                "required": ["path"]
            }),
        )])
        .with_temperature(0.2);

        let payload = build_request("gpt-4o-mini", request).expect("request should build");

        assert_eq!(payload.model, "gpt-4o-mini");
        assert_eq!(payload.messages.len(), 4);
        assert!(payload.tools.is_some());
        assert_eq!(payload.tools.as_ref().map(Vec::len), Some(1));
        assert_eq!(payload.temperature, Some(0.2f32));

        // 验证 assistant 消息包含 tool_calls
        match &payload.messages[2] {
            ChatCompletionRequestMessage::Assistant(msg) => {
                assert!(msg.tool_calls.is_some());
                assert_eq!(msg.tool_calls.as_ref().map(Vec::len), Some(1));
            }
            _ => panic!("expected assistant message"),
        }

        // 验证 tool 消息的 tool_call_id
        match &payload.messages[3] {
            ChatCompletionRequestMessage::Tool(msg) => {
                assert_eq!(msg.tool_call_id, "call_1");
            }
            _ => panic!("expected tool message"),
        }
    }

    #[test]
    fn parse_response_with_text_and_tool_call() {
        let response = CreateChatCompletionResponse {
            id: "chatcmpl-1".to_string(),
            object: "chat.completion".to_string(),
            created: 0,
            model: "gpt-4o-mini".to_string(),
            choices: vec![ChatChoice {
                index: 0,
                message: ChatCompletionResponseMessage {
                    role: async_openai::types::chat::Role::Assistant,
                    content: Some("I will inspect the file.".to_string()),
                    tool_calls: Some(vec![ChatCompletionMessageToolCalls::Function(
                        ChatCompletionMessageToolCall {
                            id: "call_1".to_string(),
                            function: FunctionCall {
                                name: "read_file".to_string(),
                                arguments: r#"{"path":"src/main.rs"}"#.to_string(),
                            },
                        },
                    )]),
                    refusal: None,
                    #[allow(deprecated)]
                    function_call: None,
                    audio: None,
                    annotations: None,
                },
                finish_reason: Some(FinishReason::ToolCalls),
                logprobs: None,
            }],
            usage: Some(CompletionUsage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                prompt_tokens_details: None,
                completion_tokens_details: None,
            }),
            service_tier: None,
            #[allow(deprecated)]
            system_fingerprint: None,
        };

        let parsed = parse_response(response).expect("response should parse");

        assert_eq!(parsed.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(parsed.usage.unwrap().total_tokens, Some(15));
        assert_eq!(parsed.message.role, Role::Assistant);
        assert_eq!(parsed.message.parts.len(), 2);
        assert_eq!(
            parsed.message.text_parts().collect::<Vec<_>>(),
            vec!["I will inspect the file."]
        );

        match &parsed.message.parts[1] {
            Part::ToolCall(call) => {
                assert_eq!(call.id, "call_1");
                assert_eq!(call.name, "read_file");
            }
            _ => panic!("expected tool call"),
        }
    }

    #[test]
    fn stream_accumulator_emits_text_and_tool_events() {
        let mut accumulator = StreamAccumulator::default();

        let chunk1 = CreateChatCompletionStreamResponse {
            id: "chatcmpl-1".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 0,
            model: "gpt-4o-mini".to_string(),
            choices: vec![ChatChoiceStream {
                index: 0,
                delta: ChatCompletionStreamResponseDelta {
                    role: Some(async_openai::types::chat::Role::Assistant),
                    content: Some("Hello".to_string()),
                    tool_calls: Some(vec![ChatCompletionMessageToolCallChunk {
                        index: 0,
                        id: Some("call_1".to_string()),
                        r#type: None,
                        function: Some(FunctionCallStream {
                            name: Some("read_file".to_string()),
                            arguments: Some("{\"path\":\"src/".to_string()),
                        }),
                    }]),
                    refusal: None,
                    #[allow(deprecated)]
                    function_call: None,
                },
                finish_reason: None,
                logprobs: None,
            }],
            usage: None,
            service_tier: None,
            #[allow(deprecated)]
            system_fingerprint: None,
        };

        let events = accumulator.apply_chunk(&chunk1).expect("chunk should parse");

        assert_eq!(
            events,
            vec![
                ResponseEvent::TextDelta("Hello".to_string()),
                ResponseEvent::ToolCallStart {
                    id: "call_1".to_string(),
                    name: "read_file".to_string(),
                },
                ResponseEvent::ToolCallArgumentsDelta {
                    id: "call_1".to_string(),
                    chunk: "{\"path\":\"src/".to_string(),
                },
            ]
        );

        let chunk2 = CreateChatCompletionStreamResponse {
            id: "chatcmpl-1".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 0,
            model: "gpt-4o-mini".to_string(),
            choices: vec![ChatChoiceStream {
                index: 0,
                delta: ChatCompletionStreamResponseDelta {
                    role: None,
                    content: Some(" world".to_string()),
                    tool_calls: Some(vec![ChatCompletionMessageToolCallChunk {
                        index: 0,
                        id: None,
                        r#type: None,
                        function: Some(FunctionCallStream {
                            name: None,
                            arguments: Some("main.rs\"}".to_string()),
                        }),
                    }]),
                    refusal: None,
                    function_call: None,
                },
                finish_reason: Some(FinishReason::ToolCalls),
                logprobs: None,
            }],
            usage: Some(CompletionUsage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                prompt_tokens_details: None,
                completion_tokens_details: None,
            }),
            service_tier: None,
            #[allow(deprecated)]
            system_fingerprint: None,
        };

        let events = accumulator.apply_chunk(&chunk2).expect("chunk should parse");

        assert_eq!(
            events,
            vec![
                ResponseEvent::TextDelta(" world".to_string()),
                ResponseEvent::ToolCallArgumentsDelta {
                    id: "call_1".to_string(),
                    chunk: "main.rs\"}".to_string(),
                },
            ]
        );

        let events = accumulator.finish().expect("finish should work");
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0],
            ResponseEvent::ToolCallDone {
                id: "call_1".to_string(),
                name: "read_file".to_string(),
                arguments: "{\"path\":\"src/main.rs\"}".to_string(),
            }
        );

        match &events[1] {
            ResponseEvent::MessageDone(response) => {
                assert_eq!(response.finish_reason.as_deref(), Some("tool_calls"));
                assert_eq!(response.usage.unwrap().total_tokens, Some(15));
                assert_eq!(
                    response.message.text_parts().collect::<Vec<_>>(),
                    vec!["Hello world"]
                );
                assert_eq!(response.message.parts.len(), 2);
            }
            _ => panic!("expected message done event"),
        }
    }

    #[test]
    fn response_stream_delegates_to_inner_stream() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime should build");
        let (tx_event, rx_event) = mpsc::channel(1);
        let mut stream = ResponseStream::new(rx_event);

        runtime.block_on(async {
            tx_event
                .send(Ok(ResponseEvent::TextDelta("hi".to_string())))
                .await
                .expect("send should succeed");
        });

        let item = runtime.block_on(async { stream.next().await.expect("event should exist") });
        assert_eq!(
            item.expect("event should be ok"),
            ResponseEvent::TextDelta("hi".to_string())
        );
    }
}
