//! Model access module.
//!
//! Defines the internal canonical item model and [`ModelProvider`] trait,
//! with an OpenAI-compatible implementation via `async-openai`: [`OpenAIProvider`].

#![allow(dead_code)]

use async_openai::Client as OpenAIClient;
use async_openai::config::OpenAIConfig;
use async_openai::error::OpenAIError;
use async_openai::types::chat::{
    ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls,
    ChatCompletionRequestAssistantMessage, ChatCompletionRequestAssistantMessageContent,
    ChatCompletionRequestMessage, ChatCompletionRequestSystemMessage,
    ChatCompletionRequestSystemMessageContent, ChatCompletionRequestToolMessage,
    ChatCompletionRequestToolMessageContent, ChatCompletionRequestUserMessage,
    ChatCompletionRequestUserMessageContent, ChatCompletionResponseMessage,
    ChatCompletionResponseStream, ChatCompletionTool, ChatCompletionTools, CompletionUsage,
    CreateChatCompletionRequest, CreateChatCompletionResponse, FinishReason, FunctionCall,
    FunctionObject,
};
use async_trait::async_trait;
use futures_util::{Stream, StreamExt};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// ==================== Item / Message Types ====================

/// Internal canonical conversation item model.
///
/// `Item` is the provider-neutral history unit used by Session / Agent.
/// Provider-specific message/block shapes are projected to and from this enum
/// inside adapter code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Message(Message),
    ToolCall(ToolCall),
    ToolResult(ToolResult),
}

impl Item {
    pub fn message(message: Message) -> Self {
        Self::Message(message)
    }

    pub fn system(text: impl Into<String>) -> Self {
        Self::Message(Message::system(text))
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self::Message(Message::user(text))
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self::Message(Message::assistant(text))
    }

    pub fn tool_call(call: ToolCall) -> Self {
        Self::ToolCall(call)
    }

    pub fn tool_result(result: ToolResult) -> Self {
        Self::ToolResult(result)
    }

    pub fn text_content(&self) -> Option<&str> {
        match self {
            Item::Message(message) => message.text_content(),
            Item::ToolCall(call) => Some(call.arguments.as_str()),
            Item::ToolResult(result) => Some(result.content.as_str()),
        }
    }

    pub fn as_tool_call(&self) -> Option<&ToolCall> {
        match self {
            Item::ToolCall(call) => Some(call),
            _ => None,
        }
    }
}

/// Internal role-based text message model.
///
/// Each variant is role-specific and only carries data valid for that role,
/// while tool calls/results are represented by sibling [`Item`] variants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    System(String),
    User(String),
    Assistant(String),
}

impl Message {
    pub fn system(text: impl Into<String>) -> Self {
        Self::System(text.into())
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self::User(text.into())
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self::Assistant(text.into())
    }

    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self::assistant(text)
    }

    /// Extract plain text from the message when available.
    pub fn text_content(&self) -> Option<&str> {
        match self {
            Message::System(t) | Message::User(t) | Message::Assistant(t) => Some(t),
        }
    }
}

/// A single tool call issued by the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON string for deferred parsing or direct passthrough to providers.
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

/// Result of executing a tool invocation.
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

// ==================== Request / Response / Streaming ====================

/// A single model invocation request.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelRequest {
    pub items: Vec<Item>,
    pub tools: Vec<ToolSpec>,
    pub temperature: Option<f32>,
}

impl ModelRequest {
    pub fn new(items: Vec<Item>) -> Self {
        Self {
            items,
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

/// A single model invocation response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelResponse {
    pub items: Vec<Item>,
    pub finish_reason: Option<String>,
    pub usage: Option<TokenUsage>,
}

/// Streaming response events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseEvent {
    TextDelta(String),
    ToolCallStart {
        id: String,
        name: String,
    },
    TextDone(String),
    ToolCallReady {
        id: String,
        name: String,
        arguments: String,
    },
    Cancelled,
    Completed {
        usage: Option<TokenUsage>,
        finish_reason: Option<String>,
    },
}

/// A model streaming response handle.
///
/// ResponseStream 只负责传递 provider 产生的事件。
/// 取消语义由 provider 转换成显式的 [`ResponseEvent::Cancelled`]。
pub struct ResponseStream {
    pub rx_event: mpsc::Receiver<Result<ResponseEvent, ModelError>>,
}

impl ResponseStream {
    pub fn new(rx_event: mpsc::Receiver<Result<ResponseEvent, ModelError>>) -> Self {
        Self { rx_event }
    }
}

impl Stream for ResponseStream {
    type Item = Result<ResponseEvent, ModelError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.rx_event.poll_recv(cx)
    }
}

/// Tool definition exposed to the upper layer.
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

/// Token usage statistics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub total_tokens: Option<u32>,
}

// ==================== Error Types ====================

/// Model invocation errors.
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

// ==================== ModelProvider trait ====================

/// Model provider abstraction.
///
/// Implement this trait for any LLM service to integrate with the upper layer
/// without modifying [`Model`].
#[async_trait]
pub trait ModelProvider: Send + Sync {
    async fn send(&self, model: &str, request: ModelRequest) -> Result<ModelResponse, ModelError>;
    async fn stream(
        &self,
        model: &str,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<ResponseStream, ModelError>;
}

// ==================== Model ====================

/// Unified model handle that delegates to a concrete [`ModelProvider`].
pub struct Model {
    provider: Box<dyn ModelProvider>,
    model: String,
}

impl Model {
    pub fn new(
        provider: Box<dyn ModelProvider>,
        model: impl Into<String>,
    ) -> Result<Self, ModelError> {
        let model = model.into();
        if model.trim().is_empty() {
            return Err(ModelError::EmptyModelName);
        }
        Ok(Self { provider, model })
    }

    pub async fn send(&self, request: ModelRequest) -> Result<ModelResponse, ModelError> {
        self.provider.send(&self.model, request).await
    }

    pub async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<ResponseStream, ModelError> {
        self.provider.stream(&self.model, request, cancel).await
    }
}

// ==================== OpenAIProvider ====================

/// OpenAI-compatible model provider backed by `async-openai`.
#[derive(Debug, Clone)]
pub struct OpenAIProvider {
    client: OpenAIClient<OpenAIConfig>,
}

impl OpenAIProvider {
    pub fn new(api_key: impl Into<String>, base_url: Option<String>) -> Result<Self, ModelError> {
        let api_key = api_key.into();
        if api_key.trim().is_empty() {
            return Err(ModelError::EmptyApiKey);
        }

        let mut config = OpenAIConfig::new().with_api_key(api_key);
        if let Some(base_url) = base_url {
            let trimmed = base_url.trim().to_string();
            if !trimmed.is_empty() {
                config = config.with_api_base(trimmed);
            }
        }

        Ok(Self {
            client: OpenAIClient::with_config(config),
        })
    }
}

#[async_trait]
impl ModelProvider for OpenAIProvider {
    async fn send(&self, model: &str, request: ModelRequest) -> Result<ModelResponse, ModelError> {
        let req = build_request(model, request)?;
        let response = self.client.chat().create(req).await?;
        parse_response(response)
    }

    async fn stream(
        &self,
        model: &str,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<ResponseStream, ModelError> {
        let req = build_request(model, request)?;
        let stream = self.client.chat().create_stream(req).await?;

        let (tx_event, rx_event) = mpsc::channel(32);

        // 参考 Codex CLI: spawned task 使用 child_token 确保取消独立
        let task_cancel = cancel.child_token();
        tokio::spawn(async move {
            let mut text = String::new();
            let mut tool_calls: Vec<PartialToolCall> = Vec::new();
            let mut finish_reason = None;
            let mut usage = None;

            let stream: ChatCompletionResponseStream = stream;
            let mut stream = std::pin::pin!(stream);

            loop {
                // 参考 Codex CLI `codex-rs/core/src/codex.rs:7052-7059`:
                // 用 tokio::select! 将 SSE stream.next() 与 cancel.cancelled() 竞争
                let result = tokio::select! {
                    _ = task_cancel.cancelled() => {
                        let _ = tx_event.send(Ok(ResponseEvent::Cancelled)).await;
                        return;
                    }
                    r = stream.next() => r,
                };

                let Some(chunk_result) = result else { break };
                let chunk = match chunk_result {
                    Ok(chunk) => chunk,
                    Err(err) => {
                        let _ = tx_event.send(Err(ModelError::OpenAI(err))).await;
                        return;
                    }
                };

                if let Some(u) = &chunk.usage {
                    usage = Some(token_usage_from_completion(u.clone()));
                }

                for choice in &chunk.choices {
                    if choice.index != 0 {
                        continue;
                    }
                    let delta = &choice.delta;
                    let fr = &choice.finish_reason;

                    if let Some(content) = &delta.content {
                        if !content.is_empty() {
                            text.push_str(content);
                            if tx_event
                                .send(Ok(ResponseEvent::TextDelta(content.clone())))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                    if let Some(tcs) = &delta.tool_calls {
                        for tc_delta in tcs {
                            let idx = tc_delta.index as usize;
                            let partial = match ensure_partial(&mut tool_calls, idx) {
                                Ok(p) => p,
                                Err(err) => {
                                    let _ = tx_event.send(Err(err)).await;
                                    return;
                                }
                            };
                            if let Some(id) = &tc_delta.id {
                                partial.id = Some(id.clone());
                            }
                            if let Some(func) = &tc_delta.function {
                                if let Some(name) = &func.name {
                                    partial.name = Some(name.clone());
                                }
                                if let Some(args) = &func.arguments {
                                    partial.arguments.push_str(args);
                                }
                            }
                            maybe_emit_start(partial, &tx_event).await;
                        }
                    }
                    if let Some(reason) = fr {
                        finish_reason = Some(finish_reason_to_string(*reason));
                    }
                }
            }

            // Stream ended — emit completed domain events, then a terminal Completed.
            if !text.is_empty() {
                if tx_event.send(Ok(ResponseEvent::TextDone(text))).await.is_err() {
                    return;
                }
            }
            for call in tool_calls {
                let id = match call.id {
                    Some(id) => id,
                    None => {
                        let _ = tx_event
                            .send(Err(ModelError::StreamProtocol(
                                "tool call id missing at stream end",
                            )))
                            .await;
                        return;
                    }
                };
                let name = match call.name {
                    Some(n) => n,
                    None => {
                        let _ = tx_event
                            .send(Err(ModelError::StreamProtocol(
                                "tool call name missing at stream end",
                            )))
                            .await;
                        return;
                    }
                };
                if tx_event
                    .send(Ok(ResponseEvent::ToolCallReady {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: call.arguments.clone(),
                    }))
                    .await
                    .is_err()
                {
                    return;
                }
            }

            let _ = tx_event
                .send(Ok(ResponseEvent::Completed {
                    finish_reason,
                    usage,
                }))
                .await;
        });

        Ok(ResponseStream::new(rx_event))
    }
}

// ==================== Streaming Helpers ====================

#[derive(Debug, Default)]
struct PartialToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
    announced_start: bool,
}

fn ensure_partial(
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
    tool_calls.get_mut(index).ok_or(ModelError::StreamProtocol(
        "tool call delta index out of bounds",
    ))
}

async fn maybe_emit_start(
    partial: &mut PartialToolCall,
    tx: &mpsc::Sender<Result<ResponseEvent, ModelError>>,
) {
    if partial.announced_start {
        return;
    }
    let (Some(id), Some(name)) = (partial.id.clone(), partial.name.clone()) else {
        return;
    };
    partial.announced_start = true;
    let _ = tx.send(Ok(ResponseEvent::ToolCallStart { id, name })).await;
}

// ==================== OpenAI Request Building ====================

fn build_request(
    model: &str,
    request: ModelRequest,
) -> Result<CreateChatCompletionRequest, ModelError> {
    let messages = items_to_openai_messages(&request.items)?;

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

fn items_to_openai_messages(
    items: &[Item],
) -> Result<Vec<ChatCompletionRequestMessage>, ModelError> {
    let mut messages = Vec::new();
    let mut index = 0;

    while index < items.len() {
        match &items[index] {
            Item::Message(Message::System(text)) => {
                messages.push(ChatCompletionRequestMessage::System(
                    ChatCompletionRequestSystemMessage {
                        content: ChatCompletionRequestSystemMessageContent::Text(text.clone()),
                        name: None,
                    },
                ));
                index += 1;
            }
            Item::Message(Message::User(text)) => {
                messages.push(ChatCompletionRequestMessage::User(
                    ChatCompletionRequestUserMessage {
                        content: ChatCompletionRequestUserMessageContent::Text(text.clone()),
                        name: None,
                    },
                ));
                index += 1;
            }
            Item::ToolResult(result) => {
                messages.push(ChatCompletionRequestMessage::Tool(
                    ChatCompletionRequestToolMessage {
                        content: ChatCompletionRequestToolMessageContent::Text(
                            result.content.clone(),
                        ),
                        tool_call_id: result.tool_call_id.clone(),
                    },
                ));
                index += 1;
            }
            Item::Message(Message::Assistant(_)) | Item::ToolCall(_) => {
                let mut text_parts = Vec::new();
                let mut tool_calls = Vec::new();

                while index < items.len() {
                    match &items[index] {
                        Item::Message(Message::Assistant(text)) => {
                            text_parts.push(text.clone());
                            index += 1;
                        }
                        Item::ToolCall(call) => {
                            tool_calls.push(ChatCompletionMessageToolCalls::Function(
                                ChatCompletionMessageToolCall {
                                    id: call.id.clone(),
                                    function: FunctionCall {
                                        name: call.name.clone(),
                                        arguments: call.arguments.clone(),
                                    },
                                },
                            ));
                            index += 1;
                        }
                        _ => break,
                    }
                }

                messages.push(ChatCompletionRequestMessage::Assistant(
                    ChatCompletionRequestAssistantMessage {
                        content: join_text_parts(text_parts)
                            .map(ChatCompletionRequestAssistantMessageContent::Text),
                        name: None,
                        tool_calls: if tool_calls.is_empty() {
                            None
                        } else {
                            Some(tool_calls)
                        },
                        ..Default::default()
                    },
                ));
            }
        }
    }

    Ok(messages)
}

fn join_text_parts(parts: Vec<String>) -> Option<String> {
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

// ==================== OpenAI Response Parsing ====================

fn parse_response(response: CreateChatCompletionResponse) -> Result<ModelResponse, ModelError> {
    let choice = response
        .choices
        .into_iter()
        .next()
        .ok_or(ModelError::EmptyChoices)?;
    let items = openai_message_to_items(choice.message)?;
    let usage = response.usage.map(token_usage_from_completion);

    Ok(ModelResponse {
        items,
        finish_reason: choice.finish_reason.map(finish_reason_to_string),
        usage,
    })
}

fn openai_message_to_items(
    message: ChatCompletionResponseMessage,
) -> Result<Vec<Item>, ModelError> {
    let mut items = Vec::new();

    if let Some(content) = message.content {
        if !content.is_empty() {
            items.push(Item::assistant(content));
        }
    }

    if let Some(tool_calls) = message.tool_calls {
        for tool_call in tool_calls {
            match tool_call {
                ChatCompletionMessageToolCalls::Function(func_call) => {
                    items.push(Item::ToolCall(ToolCall::new(
                        func_call.id,
                        func_call.function.name,
                        func_call.function.arguments,
                    )));
                }
                ChatCompletionMessageToolCalls::Custom(_) => {
                    return Err(ModelError::InvalidMessage(
                        "custom tool calls are not supported",
                    ));
                }
            }
        }
    }

    Ok(items)
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

// ==================== Tests ====================

#[cfg(test)]
mod tests {
    use super::*;
    use async_openai::types::chat::{ChatChoice, ChatCompletionMessageToolCall, FunctionCall};
    use serde_json::json;

    #[test]
    fn create_text_items() {
        let system = Message::system("You are helpful.");
        let user = Message::user("hello");
        let assistant = Message::assistant_text("hi");

        assert!(matches!(system, Message::System(ref s) if s == "You are helpful."));
        assert!(matches!(user, Message::User(ref s) if s == "hello"));
        assert!(matches!(assistant, Message::Assistant(ref s) if s == "hi"));
        assert_eq!(assistant.text_content(), Some("hi"));
    }

    #[test]
    fn create_items() {
        let text = Item::assistant("let me check");
        let call = Item::tool_call(ToolCall::new(
            "call_1",
            "read_file",
            r#"{"path":"src/main.rs"}"#,
        ));
        let result = Item::tool_result(ToolResult::new("call_1", "read_file", "fn main() {}"));

        assert_eq!(text.text_content(), Some("let me check"));
        assert!(matches!(call, Item::ToolCall(_)));
        assert!(matches!(result, Item::ToolResult(_)));
    }

    #[test]
    fn create_tool_result() {
        let result = ToolResult::new("call_1", "read_file", "fn main() {}");
        assert_eq!(result.tool_call_id, "call_1");
        assert_eq!(result.tool_name, "read_file");
        assert_eq!(result.content, "fn main() {}");
        assert!(!result.is_error);
    }

    #[test]
    fn build_request_with_tool_calls() {
        let request = ModelRequest::new(vec![
            Item::system("You are helpful."),
            Item::user("hello"),
            Item::assistant("let me check"),
            Item::tool_call(ToolCall::new(
                "call_1",
                "read_file",
                r#"{"path":"src/main.rs"}"#,
            )),
            Item::tool_result(ToolResult::new("call_1", "read_file", "file content")),
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

        match &payload.messages[2] {
            ChatCompletionRequestMessage::Assistant(msg) => {
                assert!(msg.tool_calls.is_some());
                assert_eq!(msg.tool_calls.as_ref().map(Vec::len), Some(1));
            }
            _ => panic!("expected assistant item sequence"),
        }

        match &payload.messages[3] {
            ChatCompletionRequestMessage::Tool(msg) => {
                assert_eq!(msg.tool_call_id, "call_1");
            }
            _ => panic!("expected tool result message"),
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

        assert_eq!(parsed.items.len(), 2);
        assert!(matches!(
            &parsed.items[0],
            Item::Message(Message::Assistant(text)) if text == "I will inspect the file."
        ));
        match &parsed.items[1] {
            Item::ToolCall(call) => {
                assert_eq!(call.id, "call_1");
                assert_eq!(call.name, "read_file");
            }
            _ => panic!("expected tool call"),
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

    #[test]
    fn model_delegates_to_provider() {
        struct MockProvider;

        #[async_trait::async_trait]
        impl ModelProvider for MockProvider {
            async fn send(
                &self,
                model: &str,
                _request: ModelRequest,
            ) -> Result<ModelResponse, ModelError> {
                Ok(ModelResponse {
                    items: vec![Item::assistant(format!("from {model}"))],
                    finish_reason: Some("stop".to_string()),
                    usage: None,
                })
            }

            async fn stream(
                &self,
                _model: &str,
                _request: ModelRequest,
                _cancel: CancellationToken,
            ) -> Result<ResponseStream, ModelError> {
                let (_tx, rx) = mpsc::channel(1);
                Ok(ResponseStream::new(rx))
            }
        }

        let runtime = tokio::runtime::Runtime::new().expect("runtime should build");
        let model = Model::new(Box::new(MockProvider), "test-model").expect("model should build");

        let response = runtime.block_on(model.send(ModelRequest::new(vec![])));
        let response = response.expect("send should succeed");
        assert_eq!(response.items[0].text_content(), Some("from test-model"));
    }
}
