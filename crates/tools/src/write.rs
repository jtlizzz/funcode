use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

use crate::tool::{
    PermissionRequest, Progress, RiskLevel, Tool, ToolContext,
    ToolParameters, ToolResult,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteInput {
    pub content: String,
    pub file_path: String,
}

pub struct WriteTool {
    require_permission: bool,
}

impl WriteTool {
    pub fn new() -> Self {
        Self {
            require_permission: true,
        }
    }

    pub fn with_require_permission(mut self, require: bool) -> Self {
        self.require_permission = require;
        self
    }

    fn assess_risk(&self, file_path: &str) -> RiskLevel {
        let high_risk_patterns = [
            "/etc/passwd",
            "/etc/shadow",
            ".ssh/",
            ".gnupg/",
            ".env",
            "credentials",
            "secrets",
        ];

        let medium_risk_patterns = [
            "config.json",
            "settings.json",
            ".config/",
        ];

        for pattern in high_risk_patterns {
            if file_path.contains(pattern) {
                return RiskLevel::High;
            }
        }

        for pattern in medium_risk_patterns {
            if file_path.contains(pattern) {
                return RiskLevel::Medium;
            }
        }

        RiskLevel::Low
    }

    fn parse_input(&self, args: &HashMap<String, ToolParameters>) -> Result<WriteInput> {
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("Missing 'content' parameter"))?
            .to_string();

        let file_path = args
            .get("file_path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("Missing 'file_path' parameter"))?
            .to_string();

        Ok(WriteInput { content, file_path })
    }
}

impl Default for WriteTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write content to a file at the specified path. Overwrites existing files."
    }

    fn parameters(&self) -> ToolParameters {
        serde_json::json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The content to write to the file"
                },
                "file_path": {
                    "type": "string",
                    "description": "The absolute path to the file to write"
                }
            },
            "required": ["content", "file_path"]
        })
    }

    async fn execute(
        &self,
        args: HashMap<String, ToolParameters>,
        ctx: &dyn ToolContext,
    ) -> Result<ToolResult> {
        let input = self.parse_input(&args)?;

        if self.require_permission {
            let risk_level = self.assess_risk(&input.file_path);
            let request = PermissionRequest::new(
                self.name(),
                format!("Write to: {}", input.file_path),
            )
            .with_details(format!("Content length: {} bytes", input.content.len()))
            .with_risk_level(risk_level);

            ctx.report_progress(Progress::new(format!(
                "Requesting permission to write: {}",
                input.file_path
            )))
            .await;

            let response = ctx.request_permission(request).await?;

            if !response.granted {
                return Ok(ToolResult::failure(
                    response
                        .reason
                        .unwrap_or_else(|| "Permission denied".to_string()),
                ));
            }
        }

        let path = Path::new(&input.file_path);
        
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                return Ok(ToolResult::failure(format!(
                    "Parent directory does not exist: {}",
                    parent.display()
                )));
            }
        }

        ctx.report_progress(Progress::new(format!(
            "Writing to: {}",
            input.file_path
        )))
        .await;

        tokio::fs::write(&input.file_path, &input.content).await?;

        Ok(ToolResult::success(format!(
            "Successfully wrote {} bytes to {}",
            input.content.len(),
            input.file_path
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AbortEvent, PermissionResponse};
    use tempfile::TempDir;
    use tokio::sync::broadcast;

    struct MockToolContext {
        abort_tx: broadcast::Sender<AbortEvent>,
        should_grant: bool,
    }

    impl MockToolContext {
        fn new(should_grant: bool) -> Self {
            let (abort_tx, _) = broadcast::channel(16);
            Self {
                abort_tx,
                should_grant,
            }
        }
    }

    #[async_trait]
    impl ToolContext for MockToolContext {
        async fn request_permission(
            &self,
            _request: PermissionRequest,
        ) -> Result<PermissionResponse> {
            Ok(PermissionResponse::from(self.should_grant))
        }

        fn abort_receiver(&self) -> broadcast::Receiver<AbortEvent> {
            self.abort_tx.subscribe()
        }

        async fn report_progress(&self, _progress: Progress) {}
    }

    #[test]
    fn test_write_tool_name_and_description() {
        let tool = WriteTool::new();
        assert_eq!(tool.name(), "write");
        assert!(!tool.description().is_empty());
    }

    #[test]
    fn test_write_tool_parameters() {
        let tool = WriteTool::new();
        let params = tool.parameters();
        assert!(params.is_object());
        let obj = params.as_object().unwrap();
        assert!(obj.contains_key("properties"));
        assert!(obj.contains_key("required"));
    }

    #[test]
    fn test_assess_risk_high() {
        let tool = WriteTool::new();
        assert_eq!(tool.assess_risk("/etc/passwd"), RiskLevel::High);
        assert_eq!(tool.assess_risk("/home/user/.ssh/id_rsa"), RiskLevel::High);
        assert_eq!(tool.assess_risk("/app/.env"), RiskLevel::High);
    }

    #[test]
    fn test_assess_risk_medium() {
        let tool = WriteTool::new();
        assert_eq!(tool.assess_risk("/app/config.json"), RiskLevel::Medium);
    }

    #[test]
    fn test_assess_risk_low() {
        let tool = WriteTool::new();
        assert_eq!(tool.assess_risk("/tmp/test.txt"), RiskLevel::Low);
        assert_eq!(tool.assess_risk("/home/user/file.txt"), RiskLevel::Low);
    }

    #[tokio::test]
    async fn test_write_new_file() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        let file_path_str = file_path.to_str().unwrap();

        let tool = WriteTool::new().with_require_permission(false);
        let ctx = MockToolContext::new(true);

        let mut args = HashMap::new();
        args.insert("content".to_string(), serde_json::json!("hello world"));
        args.insert("file_path".to_string(), serde_json::json!(file_path_str));

        let result = tool.execute(args, &ctx).await.unwrap();
        assert!(result.success);

        let content = tokio::fs::read_to_string(file_path).await.unwrap();
        assert_eq!(content, "hello world");
    }

    #[tokio::test]
    async fn test_write_overwrite_file() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        let file_path_str = file_path.to_str().unwrap();

        tokio::fs::write(&file_path, "old content").await.unwrap();

        let tool = WriteTool::new().with_require_permission(false);
        let ctx = MockToolContext::new(true);

        let mut args = HashMap::new();
        args.insert("content".to_string(), serde_json::json!("new content"));
        args.insert("file_path".to_string(), serde_json::json!(file_path_str));

        let result = tool.execute(args, &ctx).await.unwrap();
        assert!(result.success);

        let content = tokio::fs::read_to_string(file_path).await.unwrap();
        assert_eq!(content, "new content");
    }

    #[tokio::test]
    async fn test_write_permission_denied() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        let file_path_str = file_path.to_str().unwrap();

        let tool = WriteTool::new().with_require_permission(true);
        let ctx = MockToolContext::new(false);

        let mut args = HashMap::new();
        args.insert("content".to_string(), serde_json::json!("hello"));
        args.insert("file_path".to_string(), serde_json::json!(file_path_str));

        let result = tool.execute(args, &ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Permission denied"));
    }

    #[tokio::test]
    async fn test_write_parent_not_exists() {
        let tool = WriteTool::new().with_require_permission(false);
        let ctx = MockToolContext::new(true);

        let mut args = HashMap::new();
        args.insert("content".to_string(), serde_json::json!("hello"));
        args.insert("file_path".to_string(), serde_json::json!("/nonexistent/dir/file.txt"));

        let result = tool.execute(args, &ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Parent directory does not exist"));
    }

    #[test]
    fn test_parse_input() {
        let tool = WriteTool::new();
        let mut args = HashMap::new();
        args.insert("content".to_string(), serde_json::json!("test content"));
        args.insert("file_path".to_string(), serde_json::json!("/tmp/test.txt"));

        let input = tool.parse_input(&args).unwrap();
        assert_eq!(input.content, "test content");
        assert_eq!(input.file_path, "/tmp/test.txt");
    }

    #[test]
    fn test_parse_input_missing_content() {
        let tool = WriteTool::new();
        let mut args = HashMap::new();
        args.insert("file_path".to_string(), serde_json::json!("/tmp/test.txt"));

        let result = tool.parse_input(&args);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_input_missing_file_path() {
        let tool = WriteTool::new();
        let mut args = HashMap::new();
        args.insert("content".to_string(), serde_json::json!("test"));

        let result = tool.parse_input(&args);
        assert!(result.is_err());
    }
}
