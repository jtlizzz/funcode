//! 工具注册与分发模块。
//!
//! 这个文件负责：
//! - 定义工具特征（Tool trait）
//! - 维护可用工具列表（ToolRegistry）
//! - 将模型产生的工具调用路由到具体实现
//! - 统一工具输入输出格式
//! - 组织工具执行结果回传给 Agent

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use serde_json::Value;

use crate::model::{ToolResult, ToolSpec};

// ==================== 错误类型 ====================

/// 工具执行错误。
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// 请求的工具未注册。
    #[error("tool not found: {0}")]
    NotFound(String),
    /// 工具参数不合法。
    #[error("invalid arguments for tool '{name}': {reason}")]
    InvalidArguments { name: String, reason: String },
    /// 工具执行过程中发生错误。
    #[error("tool execution failed: {0}")]
    Execution(String),
}

// ==================== Tool 特征 ====================

/// 异步工具执行的未来类型。
type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// 工具特征，所有具体工具必须实现此接口。
pub trait Tool: Send + Sync {
    /// 工具名称，需全局唯一。
    fn name(&self) -> &str;

    /// 工具描述，供模型理解工具用途。
    fn description(&self) -> &str;

    /// 工具输入参数的 JSON Schema。
    fn input_schema(&self) -> Value;

    /// 执行工具调用，返回结果内容字符串。
    fn execute(&self, arguments: &str) -> BoxFuture<Result<String, ToolError>>;

    /// 生成供模型使用的工具定义。
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(self.name(), self.description(), self.input_schema())
    }
}

// ==================== 工具注册中心 ====================

/// 工具注册中心，维护可用工具并将调用路由到具体实现。
#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    /// 创建一个空的工具注册中心。
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个工具，如果同名工具已存在则覆盖。
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    /// 按名称获取工具的只读引用。
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|t| t.as_ref())
    }

    /// 检查指定名称的工具是否已注册。
    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// 返回已注册工具的数量。
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// 返回是否没有注册任何工具。
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// 生成所有已注册工具的定义列表，用于构建模型请求。
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|t| t.spec()).collect()
    }

    /// 执行一次工具调用。
    ///
    /// 根据工具名称路由到对应的 `Tool` 实现，将执行结果（成功或失败）
    /// 统一封装为 `ToolResult`。
    pub async fn execute(
        &self,
        tool_call_id: impl Into<String>,
        tool_name: &str,
        arguments: &str,
    ) -> ToolResult {
        let tool_call_id = tool_call_id.into();

        let Some(tool) = self.tools.get(tool_name) else {
            return ToolResult::error(
                tool_call_id,
                tool_name,
                format!("unknown tool: {tool_name}"),
            );
        };

        match tool.execute(arguments).await {
            Ok(content) => ToolResult::new(tool_call_id, tool_name, content),
            Err(err) => ToolResult::error(tool_call_id, tool_name, err.to_string()),
        }
    }
}

// ==================== 测试 ====================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 测试用工具：回显输入参数。
    struct EchoTool;

    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }

        fn description(&self) -> &str {
            "Echoes back the input arguments"
        }

        fn input_schema(&self) -> Value {
            json!({
                "type": "object",
                "properties": {
                    "message": { "type": "string" }
                },
                "required": ["message"]
            })
        }

        fn execute(&self, arguments: &str) -> BoxFuture<Result<String, ToolError>> {
            let args = arguments.to_string();
            Box::pin(async move {
                let _: Value = serde_json::from_str(&args)
                    .map_err(|e| ToolError::InvalidArguments {
                        name: "echo".to_string(),
                        reason: e.to_string(),
                    })?;
                Ok(args)
            })
        }
    }

    /// 测试用工具：总是执行失败。
    struct FailTool;

    impl Tool for FailTool {
        fn name(&self) -> &str {
            "fail"
        }

        fn description(&self) -> &str {
            "Always fails"
        }

        fn input_schema(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }

        fn execute(&self, _arguments: &str) -> BoxFuture<Result<String, ToolError>> {
            Box::pin(async {
                Err(ToolError::Execution("intentional failure".to_string()))
            })
        }
    }

    #[test]
    fn register_and_lookup_tool() {
        let mut registry = ToolRegistry::new();
        assert!(registry.is_empty());

        registry.register(Box::new(EchoTool));
        assert!(registry.contains("echo"));
        assert!(!registry.contains("unknown"));
        assert_eq!(registry.len(), 1);

        let tool = registry.get("echo").expect("echo should exist");
        assert_eq!(tool.name(), "echo");
        assert_eq!(tool.description(), "Echoes back the input arguments");
    }

    #[test]
    fn register_overwrites_same_name() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(EchoTool));
        registry.register(Box::new(EchoTool));
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn specs_collects_all_tool_definitions() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(EchoTool));
        registry.register(Box::new(FailTool));

        let specs = registry.specs();
        assert_eq!(specs.len(), 2);

        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"echo"));
        assert!(names.contains(&"fail"));

        let echo_spec = specs.iter().find(|s| s.name == "echo").unwrap();
        assert_eq!(echo_spec.description, "Echoes back the input arguments");
        assert_eq!(echo_spec.input_schema["type"], "object");
    }

    #[tokio::test]
    async fn execute_existing_tool() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(EchoTool));

        let result = registry
            .execute("call_1", "echo", r#"{"message":"hello"}"#)
            .await;

        assert_eq!(result.tool_call_id, "call_1");
        assert_eq!(result.tool_name, "echo");
        assert!(!result.is_error);
        assert_eq!(result.content, r#"{"message":"hello"}"#);
    }

    #[tokio::test]
    async fn execute_unknown_tool_returns_error() {
        let registry = ToolRegistry::new();

        let result = registry.execute("call_1", "nonexistent", "{}").await;

        assert_eq!(result.tool_call_id, "call_1");
        assert_eq!(result.tool_name, "nonexistent");
        assert!(result.is_error);
        assert!(result.content.contains("unknown tool"));
    }

    #[tokio::test]
    async fn execute_failing_tool_returns_error() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(FailTool));

        let result = registry.execute("call_2", "fail", "{}").await;

        assert_eq!(result.tool_call_id, "call_2");
        assert_eq!(result.tool_name, "fail");
        assert!(result.is_error);
        assert!(result.content.contains("intentional failure"));
    }

    #[tokio::test]
    async fn execute_tool_with_invalid_arguments() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(EchoTool));

        let result = registry.execute("call_3", "echo", "not json").await;

        assert_eq!(result.tool_call_id, "call_3");
        assert!(result.is_error);
        assert!(result.content.contains("invalid arguments"));
    }
}