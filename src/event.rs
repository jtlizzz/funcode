//! Agent event definitions.

use crate::model::TokenUsage;

// ==================== 事件定义 ====================

/// Agent 产生的所有事件。
#[derive(Debug, PartialEq)]
pub enum Event {
    // Turn 生命周期
    TurnStarted,
    /// 用户或外部取消信号中断了当前 turn。
    TurnInterrupted,
    TurnComplete {
        usage: Option<TokenUsage>,
    },

    // 模型输出（流式）
    TextDelta(String),
    TextDone(String),

    // 工具调用
    ToolCallBegin {
        id: String,
        name: String,
    },
    ToolCallEnd {
        id: String,
        name: String,
        output: String,
        is_error: bool,
    },

    // 审批请求
    ApprovalRequired {
        id: String,
        tool_name: String,
        description: String,
    },

    // 错误
    Error(String),
}
