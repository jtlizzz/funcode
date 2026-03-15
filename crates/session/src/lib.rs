use anyhow::Result;
use async_trait::async_trait;
use parking_lot::RwLock;
use provider::{ChatMessage, ChatCompletionRequest, ChatCompletionResponse, Provider};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::{broadcast, oneshot, Mutex};
use tools::{
    AbortEvent, PermissionRequest, PermissionResponse, Progress, ToolContext,
    ToolRegistry, ToolResult,
};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SessionState {
    Idle,
    Running,
    WaitingForPermission,
    Aborted,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfig {
    pub model: String,
    pub system_prompt: Option<String>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            model: "gpt-4o".to_string(),
            system_prompt: None,
            max_tokens: None,
            temperature: None,
        }
    }
}

pub struct Session {
    id: Uuid,
    config: SessionConfig,
    provider: Arc<dyn Provider>,
    tool_registry: Arc<ToolRegistry>,
    messages: RwLock<Vec<ChatMessage>>,
    state: RwLock<SessionState>,
    abort_tx: broadcast::Sender<AbortEvent>,
    pending_permission: Mutex<Option<oneshot::Sender<PermissionResponse>>>,
    event_tx: broadcast::Sender<SessionEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionEvent {
    Created { session_id: Uuid },
    MessageAdded { role: String, content: String },
    StateChanged { state: SessionState },
    PermissionRequested { request: PermissionRequest },
    PermissionGranted { granted: bool },
    ToolCalled { tool_name: String, args: serde_json::Value },
    ToolResult { tool_name: String, result: ToolResult },
    Aborted { reason: String },
    Progress { message: String },
    Error { message: String },
}

impl Session {
    pub fn new(
        config: SessionConfig,
        provider: Arc<dyn Provider>,
        tool_registry: Arc<ToolRegistry>,
    ) -> Self {
        let id = Uuid::new_v4();
        let (abort_tx, _) = broadcast::channel(16);
        let (event_tx, _) = broadcast::channel(64);

        let mut messages = Vec::new();
        if let Some(system_prompt) = &config.system_prompt {
            messages.push(ChatMessage::system(system_prompt));
        }

        Self {
            id,
            config,
            provider,
            tool_registry,
            messages: RwLock::new(messages),
            state: RwLock::new(SessionState::Idle),
            abort_tx,
            pending_permission: Mutex::new(None),
            event_tx,
        }
    }

    pub fn id(&self) -> Uuid {
        self.id
    }

    pub fn state(&self) -> SessionState {
        *self.state.read()
    }

    fn set_state(&self, state: SessionState) {
        *self.state.write() = state;
        let _ = self.event_tx.send(SessionEvent::StateChanged { state });
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.event_tx.subscribe()
    }

    pub fn subscribe_abort(&self) -> broadcast::Receiver<AbortEvent> {
        self.abort_tx.subscribe()
    }

    pub fn abort(&self, reason: impl Into<String>) {
        let reason = reason.into();
        self.set_state(SessionState::Aborted);
        let _ = self.abort_tx.send(AbortEvent::for_session(self.id.to_string(), &reason));
        let _ = self.event_tx.send(SessionEvent::Aborted { reason });
    }

    pub fn add_message(&self, message: ChatMessage) {
        let _ = self.event_tx.send(SessionEvent::MessageAdded {
            role: format!("{:?}", message.role),
            content: message.content.clone().unwrap_or_default(),
        });
        self.messages.write().push(message);
    }

    pub fn messages(&self) -> Vec<ChatMessage> {
        self.messages.read().clone()
    }

    pub async fn send_user_message(&self, content: impl Into<String>) -> Result<()> {
        self.add_message(ChatMessage::user(content));
        Ok(())
    }

    pub async fn complete(&self) -> Result<ChatCompletionResponse> {
        self.set_state(SessionState::Running);

        let mut request = ChatCompletionRequest::new(
            &self.config.model,
            self.messages(),
        );

        request = request.with_tools(self.tool_registry.to_openai_tools());

        if let Some(max_tokens) = self.config.max_tokens {
            request = request.with_max_tokens(max_tokens);
        }

        if let Some(temperature) = self.config.temperature {
            request = request.with_temperature(temperature);
        }

        let response = self.provider.complete(request).await?;

        if let Some(choice) = response.choices.first() {
            self.add_message(choice.message.clone());
        }

        self.set_state(SessionState::Completed);
        Ok(response)
    }

    pub async fn handle_tool_calls(&self, tool_calls: &[provider::ToolCall]) -> Result<Vec<ToolResult>> {
        let mut results = Vec::new();

        for tool_call in tool_calls {
            let args: std::collections::HashMap<String, serde_json::Value> =
                serde_json::from_str(&tool_call.function.arguments)?;

            let _ = self.event_tx.send(SessionEvent::ToolCalled {
                tool_name: tool_call.function.name.clone(),
                args: serde_json::Value::Object(
                    args.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
                ),
            });

            if let Some(tool) = self.tool_registry.get(&tool_call.function.name) {
                let result = tool.execute(args, self).await?;
                
                let _ = self.event_tx.send(SessionEvent::ToolResult {
                    tool_name: tool_call.function.name.clone(),
                    result: result.clone(),
                });

                results.push(result);
            } else {
                let result = ToolResult::failure(format!(
                    "Unknown tool: {}",
                    tool_call.function.name
                ));
                results.push(result);
            }
        }

        Ok(results)
    }

    pub async fn respond_to_permission(&self, granted: bool, reason: Option<String>) {
        let mut pending = self.pending_permission.lock().await;
        if let Some(tx) = pending.take() {
            let response = if granted {
                PermissionResponse::granted()
            } else {
                PermissionResponse::denied(reason.unwrap_or_else(|| "Permission denied".to_string()))
            };
            let _ = tx.send(response);
        }
        let _ = self.event_tx.send(SessionEvent::PermissionGranted { granted });
    }
}

#[async_trait]
impl ToolContext for Session {
    async fn request_permission(&self, request: PermissionRequest) -> Result<PermissionResponse> {
        self.set_state(SessionState::WaitingForPermission);
        
        let _ = self.event_tx.send(SessionEvent::PermissionRequested {
            request: request.clone(),
        });

        let (tx, rx) = oneshot::channel();
        *self.pending_permission.lock().await = Some(tx);

        let response = rx.await.unwrap_or_else(|_| {
            PermissionResponse::denied("Permission request cancelled")
        });

        self.set_state(SessionState::Running);
        Ok(response)
    }

    fn abort_receiver(&self) -> broadcast::Receiver<AbortEvent> {
        self.abort_tx.subscribe()
    }

    async fn report_progress(&self, progress: Progress) {
        let _ = self.event_tx.send(SessionEvent::Progress {
            message: progress.message,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use provider::{OpenAIProvider, ProviderConfig};

    fn create_test_session() -> Session {
        let config = SessionConfig::default();
        let provider_config = ProviderConfig::new("test-key");
        let provider = Arc::new(OpenAIProvider::new(provider_config).unwrap());
        let tool_registry = Arc::new(ToolRegistry::new());
        Session::new(config, provider, tool_registry)
    }

    #[test]
    fn test_session_creation() {
        let session = create_test_session();
        assert_eq!(session.state(), SessionState::Idle);
    }

    #[test]
    fn test_session_state_change() {
        let session = create_test_session();
        session.set_state(SessionState::Running);
        assert_eq!(session.state(), SessionState::Running);
    }

    #[test]
    fn test_session_add_message() {
        let session = create_test_session();
        session.add_message(ChatMessage::user("Hello"));
        let messages = session.messages();
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn test_session_abort() {
        let session = create_test_session();
        session.abort("Test abort");
        assert_eq!(session.state(), SessionState::Aborted);
    }

    #[test]
    fn test_session_subscribe() {
        let session = create_test_session();
        let mut rx = session.subscribe();
        
        session.set_state(SessionState::Running);
        
        if let Ok(SessionEvent::StateChanged { state }) = rx.try_recv() {
            assert_eq!(state, SessionState::Running);
        }
    }
}
