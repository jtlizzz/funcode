use anyhow::{anyhow, Result};
use async_trait::async_trait;
use crate::{Tool, ToolParameters, ToolResult};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use tokio::process::Command;

const DEFAULT_TIMEOUT_SECS: u64 = 120;

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
}

impl BashTool {
    pub fn new() -> Self {
        Self {
            allowed_commands: None,
            default_workdir: None,
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

    fn is_command_allowed(&self, command: &str) -> bool {
        if let Some(allowed) = &self.allowed_commands {
            let cmd_name = command.split_whitespace().next().unwrap_or("");
            allowed
                .iter()
                .any(|a| cmd_name == a || cmd_name.starts_with(a))
        } else {
            true
        }
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

    async fn execute(&self, args: HashMap<String, ToolParameters>) -> Result<ToolResult> {
        let input = self.parse_input(&args)?;

        if !self.is_command_allowed(&input.command) {
            return Ok(ToolResult::failure(format!(
                "Command not allowed: {}",
                input.command
            )));
        }

        let timeout = Duration::from_secs(input.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS));

        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg(&input.command);

        let workdir = input.workdir.as_ref().or(self.default_workdir.as_ref());
        if let Some(dir) = workdir {
            cmd.current_dir(dir);
        }

        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let result = tokio::time::timeout(timeout, async {
            let output = cmd.output().await?;
            Ok::<_, anyhow::Error>(output)
        })
        .await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();

                if output.status.success() {
                    let result = if stdout.is_empty() && stderr.is_empty() {
                        "Command completed successfully (no output)".to_string()
                    } else if stderr.is_empty() {
                        stdout
                    } else if stdout.is_empty() {
                        format!("stderr: {}", stderr)
                    } else {
                        format!("{}\nstderr: {}", stdout, stderr)
                    };
                    Ok(ToolResult::success(result.trim()))
                } else {
                    let code = output.status.code().unwrap_or(-1);
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
            Ok(Err(e)) => Ok(ToolResult::failure(format!("Failed to execute: {}", e))),
            Err(_) => Ok(ToolResult::failure(format!(
                "Command timed out after {} seconds",
                timeout.as_secs()
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[tokio::test]
    async fn test_bash_echo() {
        let tool = BashTool::new();
        let mut args = HashMap::new();
        args.insert(
            "command".to_string(),
            serde_json::json!("echo 'hello world'"),
        );

        let result = tool.execute(args).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output, "hello world");
    }

    #[tokio::test]
    async fn test_bash_with_workdir() {
        let tool = BashTool::new();
        let mut args = HashMap::new();
        args.insert("command".to_string(), serde_json::json!("pwd"));
        args.insert("workdir".to_string(), serde_json::json!("/tmp"));

        let result = tool.execute(args).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output, "/tmp");
    }

    #[tokio::test]
    async fn test_bash_command_not_found() {
        let tool = BashTool::new();
        let mut args = HashMap::new();
        args.insert(
            "command".to_string(),
            serde_json::json!("nonexistent_command_xyz"),
        );

        let result = tool.execute(args).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Exit code"));
    }

    #[tokio::test]
    async fn test_bash_timeout() {
        let tool = BashTool::new();
        let mut args = HashMap::new();
        args.insert("command".to_string(), serde_json::json!("sleep 10"));
        args.insert("timeout".to_string(), serde_json::json!(1));

        let result = tool.execute(args).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("timed out"));
    }

    #[tokio::test]
    async fn test_bash_allowed_commands() {
        let tool = BashTool::new().with_allowed_commands(vec!["echo".to_string()]);

        let mut args = HashMap::new();
        args.insert("command".to_string(), serde_json::json!("echo 'allowed'"));
        let result = tool.execute(args).await.unwrap();
        assert!(result.success);

        let mut args = HashMap::new();
        args.insert("command".to_string(), serde_json::json!("ls"));
        let result = tool.execute(args).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("not allowed"));
    }

    #[tokio::test]
    async fn test_bash_stderr_capture() {
        let tool = BashTool::new();
        let mut args = HashMap::new();
        args.insert("command".to_string(), serde_json::json!("echo 'error' >&2"));

        let result = tool.execute(args).await.unwrap();
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
