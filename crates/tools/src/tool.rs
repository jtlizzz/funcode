use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::sync::broadcast;

pub type ToolParameters = serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub success: bool,
    pub output: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ToolResult {
    pub fn success(output: impl Into<String>) -> Self {
        Self {
            success: true,
            output: output.into(),
            error: None,
        }
    }

    pub fn failure(error: impl Into<String>) -> Self {
        Self {
            success: false,
            output: String::new(),
            error: Some(error.into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRequest {
    pub tool_name: String,
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    pub risk_level: RiskLevel,
}

impl PermissionRequest {
    pub fn new(tool_name: impl Into<String>, action: impl Into<String>) -> Self {
        Self {
            tool_name: tool_name.into(),
            action: action.into(),
            details: None,
            risk_level: RiskLevel::Medium,
        }
    }

    pub fn with_details(mut self, details: impl Into<String>) -> Self {
        self.details = Some(details.into());
        self
    }

    pub fn with_risk_level(mut self, level: RiskLevel) -> Self {
        self.risk_level = level;
        self
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionResponse {
    pub granted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl PermissionResponse {
    pub fn granted() -> Self {
        Self {
            granted: true,
            reason: None,
        }
    }

    pub fn denied(reason: impl Into<String>) -> Self {
        Self {
            granted: false,
            reason: Some(reason.into()),
        }
    }
}

impl From<bool> for PermissionResponse {
    fn from(granted: bool) -> Self {
        if granted {
            Self::granted()
        } else {
            Self::denied("Permission denied")
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbortEvent {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub reason: String,
}

impl AbortEvent {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            session_id: None,
            reason: reason.into(),
        }
    }

    pub fn for_session(session_id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            session_id: Some(session_id.into()),
            reason: reason.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Progress {
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percentage: Option<u8>,
}

impl Progress {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            percentage: None,
        }
    }

    pub fn with_percentage(mut self, percentage: u8) -> Self {
        self.percentage = Some(percentage.min(100));
        self
    }
}

#[async_trait]
pub trait ToolContext: Send + Sync {
    async fn request_permission(&self, request: PermissionRequest) -> Result<PermissionResponse>;

    fn abort_receiver(&self) -> broadcast::Receiver<AbortEvent>;

    async fn report_progress(&self, progress: Progress);
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;

    fn description(&self) -> &str;

    fn parameters(&self) -> ToolParameters;

    async fn execute(
        &self,
        args: HashMap<String, ToolParameters>,
        ctx: &dyn ToolContext,
    ) -> Result<ToolResult>;
}

pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|t| t.as_ref())
    }

    pub fn list(&self) -> Vec<&dyn Tool> {
        self.tools.values().map(|t| t.as_ref()).collect()
    }

    pub fn to_openai_tools(&self) -> Vec<provider::ToolDefinition> {
        self.tools
            .values()
            .map(|t| {
                provider::ToolDefinition::new(t.name(), t.parameters())
                    .with_description(t.description())
            })
            .collect()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}
