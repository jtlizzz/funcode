//! 模型接入模块。
//!
//! 这个文件当前先定义内部统一消息模型，后续 provider 适配层
//! 统一把这些消息转换成各家模型 API 所需的请求结构。

#![allow(dead_code)]

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

#[cfg(test)]
mod tests {
    use super::*;

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

        assert_eq!(message.text_parts().collect::<Vec<_>>(), vec!["first", "second"]);
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
}
