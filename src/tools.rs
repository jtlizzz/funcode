//! 工具注册与分发模块。
//!
//! 这个文件负责：
//! - 定义工具特征（Tool trait）
//! - 维护可用工具列表（ToolRegistry）
//! - 将模型产生的工具调用路由到具体实现
//! - 统一工具输入输出格式
//! - 组织工具执行结果回传给 Agent

use std::collections::HashMap;
use std::path::Path;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::model::{ToolResult, ToolSpec};

// ==================== 错误类型 ====================

/// 工具执行错误。
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// 工具参数不合法。
    #[error("invalid arguments: {0}")]
    Arguments(String),
    /// 工具执行过程中发生错误。
    #[error("{0}")]
    Execution(String),
}

impl From<std::io::Error> for ToolError {
    fn from(e: std::io::Error) -> Self {
        ToolError::Execution(e.to_string())
    }
}

// ==================== 参数解析 ====================

/// 统一参数解析函数，将 JSON 字符串反序列化为强类型 struct。
pub fn parse_arguments<T>(arguments: &str, tool_name: &str) -> Result<T, ToolError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_str(arguments).map_err(|err| ToolError::Arguments(format!("{tool_name}: {err}")))
}

// ==================== Tool 特征 ====================

/// 从实现了 `JsonSchema` 的类型自动生成精简 JSON Schema。
///
/// 去除 `$schema`、`title`、顶层 `description`、`format`、`minimum` 等
/// 模型调用不需要的冗余字段；`Option<T>` 只保留内部类型，不追加 null。
pub fn schema_for<T: JsonSchema>() -> Value {
    let mut settings = schemars::r#gen::SchemaSettings::default();
    settings.meta_schema = None;
    settings.option_add_null_type = false;
    let generator = settings.into_generator();
    let root_schema = generator.into_root_schema_for::<T>();
    let mut value =
        serde_json::to_value(&root_schema).expect("schema serialization should never fail");
    clean_schema(&mut value);
    value
}

/// 移除模型调用不需要的冗余 schema 字段。
fn clean_schema(value: &mut Value) {
    let Some(obj) = value.as_object_mut() else { return };
    obj.remove("title");
    obj.remove("description");

    if let Some(props) = obj.get_mut("properties").and_then(|p| p.as_object_mut()) {
        for prop in props.values_mut() {
            if let Some(prop_obj) = prop.as_object_mut() {
                prop_obj.remove("format");
                prop_obj.remove("minimum");
            }
        }
    }
}

/// 工具特征，所有具体工具必须实现此接口。
#[async_trait]
pub trait Tool: Send + Sync {
    /// 工具名称，需全局唯一。
    fn name(&self) -> &str;

    /// 工具描述，供模型理解工具用途。
    fn description(&self) -> &str;

    /// 工具输入参数的 JSON Schema，由 `schema_for::<Input>()` 自动生成。
    fn parameters(&self) -> Value;

    /// 执行工具调用，返回结果内容字符串。
    async fn execute(&self, arguments: &str) -> Result<String, ToolError>;

    /// 生成供模型使用的工具定义。
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(self.name(), self.description(), self.parameters())
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
        self.tools.len() == 0
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

// ==================== FileReadTool ====================

/// 文件读取工具的输入参数。
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FileReadInput {
    /// 文件的绝对路径。
    #[schemars(description = "The absolute path to the file to read")]
    pub file_path: String,
    /// 起始行号（从 0 开始），可选。
    #[schemars(description = "Line number to start reading from (0-based)")]
    pub offset: Option<usize>,
    /// 读取的最大行数，可选。
    #[schemars(description = "Maximum number of lines to read")]
    pub limit: Option<usize>,
}

/// 文件读取工具。
pub struct FileReadTool;

impl FileReadTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for FileReadTool {
    fn name(&self) -> &str {
        "Read"
    }

    fn description(&self) -> &str {
        "Reads a text file from the local filesystem."
    }

    fn parameters(&self) -> Value {
        schema_for::<FileReadInput>()
    }

    async fn execute(&self, arguments: &str) -> Result<String, ToolError> {
        let args: FileReadInput = parse_arguments(arguments, "Read")?;

        let path = Path::new(&args.file_path);
        let content = tokio::fs::read_to_string(path).await?;

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();
        let start = args.offset.unwrap_or(0).min(total_lines);
        let end = args
            .limit
            .map(|l| (start + l).min(total_lines))
            .unwrap_or(total_lines);

        let selected: Vec<&str> = lines[start..end].to_vec();

        Ok(selected.join("\n"))
    }
}

// ==================== FileEditTool ====================

/// 文件编辑工具的输入参数。
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FileEditInput {
    /// 要修改的文件的绝对路径。
    #[schemars(description = "The absolute path to the file to modify")]
    pub file_path: String,
    /// 要替换的原始文本。
    #[schemars(description = "The text to replace")]
    pub old_string: String,
    /// 替换后的文本。
    #[schemars(description = "The text to replace it with (must be different from old_string)")]
    pub new_string: String,
    /// 是否替换所有匹配项，默认 false。
    #[schemars(description = "Replace all occurrences of old_string")]
    pub replace_all: Option<bool>,
}

/// 文件编辑工具。
pub struct FileEditTool;

impl FileEditTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for FileEditTool {
    fn name(&self) -> &str {
        "Edit"
    }

    fn description(&self) -> &str {
        "Performs exact string replacements in files. Supports single or global replacement."
    }

    fn parameters(&self) -> Value {
        schema_for::<FileEditInput>()
    }

    async fn execute(&self, arguments: &str) -> Result<String, ToolError> {
        let args: FileEditInput = parse_arguments(arguments, "Edit")?;

        if args.old_string == args.new_string {
            return Err(ToolError::Execution(
                "old_string and new_string must be different".to_string(),
            ));
        }

        let path = Path::new(&args.file_path);
        let content = tokio::fs::read_to_string(path).await?;

        let replace_all = args.replace_all.unwrap_or(false);
        let (new_content, count) = if replace_all {
            let count = content.matches(&args.old_string).count();
            if count == 0 {
                return Err(ToolError::Execution("old_string not found in file".to_string()));
            }
            (content.replace(&args.old_string, &args.new_string), count)
        } else {
            match content.find(&args.old_string) {
                Some(_) => (content.replacen(&args.old_string, &args.new_string, 1), 1),
                None => {
                    return Err(ToolError::Execution(
                        "old_string not found in file".to_string(),
                    ))
                }
            }
        };

        tokio::fs::write(path, &new_content).await?;

        Ok(format!(
            "replaced {} occurrence(s) in {}",
            count, args.file_path
        ))
    }
}

// ==================== FileWriteTool ====================

/// 文件写入工具的输入参数。
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FileWriteInput {
    /// 文件的绝对路径。
    #[schemars(description = "The absolute path to the file to write (must be absolute, not relative)")]
    pub file_path: String,
    /// 要写入的内容。
    #[schemars(description = "The content to write to the file")]
    pub content: String,
}

/// 文件写入工具。
pub struct FileWriteTool;

impl FileWriteTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for FileWriteTool {
    fn name(&self) -> &str {
        "Write"
    }

    fn description(&self) -> &str {
        "Writes content to a file, creating it if it doesn't exist or overwriting if it does."
    }

    fn parameters(&self) -> Value {
        schema_for::<FileWriteInput>()
    }

    async fn execute(&self, arguments: &str) -> Result<String, ToolError> {
        let args: FileWriteInput = parse_arguments(arguments, "Write")?;

        let path = Path::new(&args.file_path);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(path, &args.content).await?;

        Ok(format!(
            "wrote {} bytes to {}",
            args.content.len(),
            args.file_path
        ))
    }
}

// ==================== 测试 ====================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- 测试用工具 ----

    #[derive(Debug, Deserialize, JsonSchema)]
    struct EchoInput {
        message: String,
    }

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }

        fn description(&self) -> &str {
            "Echoes back the input arguments"
        }

        fn parameters(&self) -> Value {
            schema_for::<EchoInput>()
        }

        async fn execute(&self, arguments: &str) -> Result<String, ToolError> {
            let _: EchoInput = parse_arguments(arguments, "echo")?;
            Ok(arguments.to_string())
        }
    }

    struct FailTool;

    #[async_trait]
    impl Tool for FailTool {
        fn name(&self) -> &str {
            "fail"
        }

        fn description(&self) -> &str {
            "Always fails"
        }

        fn parameters(&self) -> Value {
            schema_for::<EchoInput>()
        }

        async fn execute(&self, _arguments: &str) -> Result<String, ToolError> {
            Err(ToolError::Execution("intentional failure".to_string()))
        }
    }

    // ---- 注册中心测试 ----

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

    // ---- 执行测试 ----

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

    // ---- 参数解析测试 ----

    #[test]
    fn parse_arguments_valid_json() {
        #[derive(Deserialize, Debug, PartialEq)]
        struct Args {
            message: String,
        }

        let args: Args = parse_arguments(r#"{"message":"hello"}"#, "test").unwrap();
        assert_eq!(args.message, "hello");
    }

    #[test]
    fn parse_arguments_invalid_json() {
        #[derive(Deserialize, Debug)]
        struct Args {
            message: String,
        }

        let err = parse_arguments::<Args>("not json", "test").unwrap_err();
        assert!(err.to_string().contains("invalid arguments"));
    }

    // ---- 文件工具 parameters 序列化测试 ----

    #[test]
    fn file_read_tool_parameters() {
        let schema = FileReadTool::new().parameters();

        let expected = json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "The absolute path to the file to read"
                },
                "offset": {
                    "type": "integer",
                    "description": "Line number to start reading from (0-based)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to read"
                }
            },
            "required": ["file_path"]
        });

        assert_eq!(schema, expected);
    }

    #[test]
    fn file_edit_tool_parameters() {
        let schema = FileEditTool::new().parameters();

        let expected = json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "The absolute path to the file to modify"
                },
                "old_string": {
                    "type": "string",
                    "description": "The text to replace"
                },
                "new_string": {
                    "type": "string",
                    "description": "The text to replace it with (must be different from old_string)"
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "Replace all occurrences of old_string"
                }
            },
            "required": ["file_path", "new_string", "old_string"]
        });

        assert_eq!(schema, expected);
    }

    #[test]
    fn file_write_tool_parameters() {
        let schema = FileWriteTool::new().parameters();

        let expected = json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "The absolute path to the file to write (must be absolute, not relative)"
                },
                "content": {
                    "type": "string",
                    "description": "The content to write to the file"
                }
            },
            "required": ["content", "file_path"]
        });

        assert_eq!(schema, expected);
    }
}
