//! funcode library

pub mod agent;
pub mod approval;
pub mod event;
pub mod model;
pub mod session;
pub mod tools;

// Re-export commonly used types at crate root
pub use agent::{Agent, AgentHandle, Op, TurnOutcome};
pub use event::Event;
pub use model::{
    Item, Message, Model, ModelError, ModelRequest, OpenAIProvider, TokenUsage, ToolSpec,
};
pub use session::Session;
pub use tools::{BashTool, FileReadTool, Tool, ToolContext, ToolRegistry};
