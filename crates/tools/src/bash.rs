use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::tool::{
    PermissionRequest, Progress, RiskLevel, Tool, ToolContext,
    ToolParameters, ToolResult,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BashInput {
    pub command: String,
    #[serde(default)]
    pub timeout: Option<u64>,
    #[serde(default)]
    pub workdir: Option<String>,
}

pub struct BashTool {
    allowed_commands: Option<Vec<String>>,
    default_workdir: Option<String>,
    require_permission: bool,
}

impl BashTool {
    pub fn new() -> Self {
        Self {
            allowed_commands: None,
            default_workdir: None,
            require_permission: true,
        }
    }

    pub fn with_allowed_commands(mut self, commands: Vec<String>) -> Self {
        self.allowed_commands = Some(commands);
        self
    }

    pub fn with_default_workdir(mut self, workdir: impl Into<String>) -> Self {
        self.default_workdir = Some(workdir.into());
        self
    }

    pub fn with_require_permission(mut self, require: bool) -> Self {
        self.require_permission = require;
        self
    }

    fn is_command_allowed(&self, command: &str) -> bool {
        if let Some(allowed) = &self.allowed_commands {
            let cmd_name = command.split_whitespace().next().unwrap_or("");
            allowed.iter().any(|a| cmd_name == a)
        } else {
            true
        }
    }

    fn assess_risk(&self, command: &str) -> RiskLevel {
        let high_risk_patterns = [
            "rm -rf",
            "rm -r",
            "sudo",
            "chmod",
            "chown",
            "mkfs",
            "dd if=",
            "> /dev/",
            "kill -9",
            "pkill",
            "shutdown",
            "reboot",
            "init 0",
            "init 6",
        ];

        let medium_risk_patterns = [
            "rm",
            "mv",
            "cp -r",
            "git push",
            "git reset --hard",
            "npm publish",
            "cargo publish",
            "docker rm",
            "docker rmi",
            "kubectl delete",
        ];

        for pattern in high_risk_patterns {
            if command.contains(pattern) {
                return RiskLevel::High;
            }
        }

        for pattern in medium_risk_patterns {
            if command.contains(pattern) {
                return RiskLevel::Medium;
            }
        }

        RiskLevel::Low
    }

    fn parse_input(&self, args: &HashMap<String, ToolParameters>) -> Result<BashInput> {
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("Missing 'command' parameter"))?
            .to_string();

        let timeout = args.get("timeout").and_then(|v| v.as_u64());

        let workdir = args
            .get("workdir")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        Ok(BashInput {
            command,
            timeout,
            workdir,
        })
    }
}

impl Default for BashTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Execute a bash command and return the output. Use this for shell operations like file manipulation, system commands, etc."
    }

    fn parameters(&self) -> ToolParameters {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The bash command to execute"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in seconds (default: 120)"
                },
                "workdir": {
                    "type": "string",
                    "description": "Working directory for the command"
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(
        &self,
        args: HashMap<String, ToolParameters>,
        ctx: &dyn ToolContext,
    ) -> Result<ToolResult> {
        let input = self.parse_input(&args)?;

        if !self.is_command_allowed(&input.command) {
            return Ok(ToolResult::failure(format!(
                "Command not allowed: {}",
                input.command
            )));
        }

        if self.require_permission {
            let risk_level = self.assess_risk(&input.command);
            let request = PermissionRequest::new(self.name(), format!("Execute: {}", input.command))
                .with_details(format!(
                    "Working directory: {}",
                    input.workdir.as_deref().unwrap_or("current")
                ))
                .with_risk_level(risk_level);

            ctx.report_progress(Progress::new(format!(
                "Requesting permission to execute: {}",
                input.command
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

        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg(&input.command);

        let workdir = input.workdir.as_ref().or(self.default_workdir.as_ref());
        if let Some(dir) = workdir {
            cmd.current_dir(dir);
        }

        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let mut child = cmd.spawn()?;

        let mut abort_rx = ctx.abort_receiver();

        
        ctx.report_progress(Progress::new(format!("Executing: {}", input.command)))
            .await;

        let timeout_duration = tokio::time::Duration::from_secs(input.timeout.unwrap_or(120));

        let result = tokio::select! {
            result = child.wait() => {
                match result {
                    Ok(status) => {
                        let stdout = match child.stdout.take() {
                            Some(mut handle) => {
                                let mut buf = String::new();
                                AsyncReadExt::read_to_string(&mut handle, &mut buf).await?;
                                buf
                            }
                            None => String::new(),
                        };
                        let stderr = match child.stderr.take() {
                            Some(mut handle) => {
                                let mut buf = String::new();
                                AsyncReadExt::read_to_string(&mut handle, &mut buf).await?;
                                buf
                            }
                            None => String::new(),
                        };

                        if status.success() {
                            let output = if stdout.is_empty() && stderr.is_empty() {
                                "Command completed successfully (no output)".to_string()
                            } else if stderr.is_empty() {
                                stdout
                            } else if stdout.is_empty() {
                                format!("stderr: {}", stderr)
                            } else {
                                format!("{}\nstderr: {}", stdout, stderr)
                            };
                            Ok(ToolResult::success(output.trim()))
                        } else {
                            let code = status.code().unwrap_or(-1);
                            let msg = if stdout.is_empty() {
                                stderr
                            } else {
                                format!("{}\n{}", stdout, stderr)
                            };
                            Ok(ToolResult::failure(format!(
                                "Exit code {}: {}",
                                code,
                                msg.trim()
                            )))
                        }
                    }
                    Err(e) => Ok(ToolResult::failure(format!("Failed to execute: {}", e))),
                }
            }

            _ = tokio::time::sleep(timeout_duration) => {
                let _ = child.kill().await;
                Ok(ToolResult::failure(format!(
                    "Command timed out after {} seconds",
                    timeout_duration.as_secs()
                )))
            }

            _ = abort_rx.recv() => {
                let _ = child.kill().await;
                Ok(ToolResult::failure("Command aborted by user"))
            }
        };

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AbortEvent, PermissionResponse};
    use std::sync::Arc;
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

        fn abort(&self, reason: &str) {
            let _ = self.abort_tx.send(AbortEvent::new(reason));
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
    fn test_bash_tool_name_and_description() {
        let tool = BashTool::new();
        assert_eq!(tool.name(), "bash");
        assert!(!tool.description().is_empty());
    }

    #[test]
    fn test_bash_tool_parameters() {
        let tool = BashTool::new();
        let params = tool.parameters();
        assert!(params.is_object());
        let obj = params.as_object().unwrap();
        assert!(obj.contains_key("properties"));
        assert!(obj.contains_key("required"));
    }

    #[test]
    fn test_assess_risk_high() {
        let tool = BashTool::new();
        assert_eq!(tool.assess_risk("rm -rf /"), RiskLevel::High);
        assert_eq!(tool.assess_risk("sudo apt install"), RiskLevel::High);
        assert_eq!(tool.assess_risk("dd if=/dev/zero of=/dev/sda"), RiskLevel::High);
    }

    #[test]
    fn test_assess_risk_medium() {
        let tool = BashTool::new();
        assert_eq!(tool.assess_risk("rm file.txt"), RiskLevel::Medium);
        assert_eq!(tool.assess_risk("git push origin main"), RiskLevel::Medium);
    }

    #[test]
    fn test_assess_risk_low() {
        let tool = BashTool::new();
        assert_eq!(tool.assess_risk("ls -la"), RiskLevel::Low);
        assert_eq!(tool.assess_risk("echo hello"), RiskLevel::Low);
        assert_eq!(tool.assess_risk("cat file.txt"), RiskLevel::Low);
    }

    #[tokio::test]
    async fn test_bash_echo_with_permission() {
        let tool = BashTool::new().with_require_permission(false);
        let ctx = MockToolContext::new(true);

        let mut args = HashMap::new();
        args.insert("command".to_string(), serde_json::json!("echo 'hello world'"));

        let result = tool.execute(args, &ctx).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output, "hello world");
    }

    #[tokio::test]
    async fn test_bash_permission_denied() {
        let tool = BashTool::new().with_require_permission(true);
        let ctx = MockToolContext::new(false);

        let mut args = HashMap::new();
        args.insert("command".to_string(), serde_json::json!("echo 'hello'"));

        let result = tool.execute(args, &ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Permission denied"));
    }

    #[tokio::test]
    async fn test_bash_command_not_found() {
        let tool = BashTool::new().with_require_permission(false);
        let ctx = MockToolContext::new(true);

        let mut args = HashMap::new();
        args.insert("command".to_string(), serde_json::json!("nonexistent_command_xyz"));

        let result = tool.execute(args, &ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Exit code"));
    }

    #[tokio::test]
    async fn test_bash_allowed_commands() {
        let tool = BashTool::new()
            .with_allowed_commands(vec!["echo".to_string()])
            .with_require_permission(false);
        let ctx = MockToolContext::new(true);

        let mut args = HashMap::new();
        args.insert("command".to_string(), serde_json::json!("echo 'allowed'"));
        let result = tool.execute(args, &ctx).await.unwrap();
        assert!(result.success);

        let mut args = HashMap::new();
        args.insert("command".to_string(), serde_json::json!("ls"));
        let result = tool.execute(args, &ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("not allowed"));
    }

    #[tokio::test]
    async fn test_bash_abort() {
        let tool = BashTool::new().with_require_permission(false);
        let ctx = Arc::new(MockToolContext::new(true));
        let ctx_clone = ctx.clone();

        let mut args = HashMap::new();
        args.insert("command".to_string(), serde_json::json!("sleep 5"));

        let handle = tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            ctx_clone.abort("User cancelled");
        });

        let result = tool.execute(args, ctx.as_ref()).await.unwrap();
        handle.abort();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("aborted"));
    }

    #[tokio::test]
    async fn test_bash_with_workdir() {
        let tool = BashTool::new().with_require_permission(false);
        let ctx = MockToolContext::new(true);

        let mut args = HashMap::new();
        args.insert("command".to_string(), serde_json::json!("pwd"));
        args.insert("workdir".to_string(), serde_json::json!("/tmp"));

        let result = tool.execute(args, &ctx).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output, "/tmp");
    }

    #[tokio::test]
    async fn test_bash_timeout() {
        let tool = BashTool::new().with_require_permission(false);
        let ctx = MockToolContext::new(true);

        let mut args = HashMap::new();
        args.insert("command".to_string(), serde_json::json!("sleep 10"));
        args.insert("timeout".to_string(), serde_json::json!(1));

        let result = tool.execute(args, &ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("timed out"));
    }

    #[tokio::test]
    async fn test_bash_stderr_capture() {
        let tool = BashTool::new().with_require_permission(false);
        let ctx = MockToolContext::new(true);

        let mut args = HashMap::new();
        args.insert("command".to_string(), serde_json::json!("echo 'error' >&2"));

        let result = tool.execute(args, &ctx).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("error"));
    }

    #[test]
    fn test_parse_input() {
        let tool = BashTool::new();
        let mut args = HashMap::new();
        args.insert("command".to_string(), serde_json::json!("ls -la"));
        args.insert("timeout".to_string(), serde_json::json!(30));
        args.insert("workdir".to_string(), serde_json::json!("/home"));

        let input = tool.parse_input(&args).unwrap();
        assert_eq!(input.command, "ls -la");
        assert_eq!(input.timeout, Some(30));
        assert_eq!(input.workdir, Some("/home".to_string()));
    }

    #[test]
    fn test_parse_input_missing_command() {
        let tool = BashTool::new();
        let args = HashMap::new();
        let result = tool.parse_input(&args);
        assert!(result.is_err());
    }
}
