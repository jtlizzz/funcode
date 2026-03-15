//! Provider 模块 - 提供 LLM API 调用能力
//!
//! 本模块提供了与大语言模型 (LLM) API 交互的抽象和实现。
//! 目前支持 OpenAI 兼容的 API (如 OpenAI、Azure OpenAI 等)。
//!
//! # 核心组件
//!
//! - [`Provider`] trait: 定义 LLM 提供商的通用接口
//! - [`OpenAIProvider`]: OpenAI API 的具体实现
//! - [`ProviderConfig`]: 配置 API 密钥、基础 URL、模型等
//! - [`ProviderFactory`]: 创建不同提供商实例的工厂
//!
//! # 快速开始
//!
//! ## 基本用法
//!
//! ```rust,no_run
//! use provider::{ProviderConfig, OpenAIProvider, Provider, ChatCompletionRequest, ChatMessage};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     // 创建配置
//!     let config = ProviderConfig::new("your-api-key")
//!         .with_model("gpt-4o");
//!     
//!     // 创建 provider
//!     let provider = OpenAIProvider::new(config)?;
//!     
//!     // 构建请求
//!     let request = ChatCompletionRequest::new("gpt-4o", vec![
//!         ChatMessage::system("You are a helpful assistant."),
//!         ChatMessage::user("Hello!"),
//!     ]);
//!     
//!     // 发送请求
//!     let response = provider.complete(request).await?;
//!     
//!     // 获取回复
//!     if let Some(content) = &response.choices[0].message.content {
//!         println!("Assistant: {}", content);
//!     }
//!     
//!     Ok(())
//! }
//! ```
//!
//! ## 使用流式输出
//!
//! ```rust,no_run
//! use provider::{ProviderConfig, OpenAIProvider, Provider, ChatCompletionRequest, ChatMessage};
//! use futures::StreamExt;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let config = ProviderConfig::new("your-api-key");
//!     let provider = OpenAIProvider::new(config)?;
//!     
//!     let request = ChatCompletionRequest::new("gpt-4o", vec![
//!         ChatMessage::user("Tell me a story."),
//!     ]);
//!     
//!     // 获取流式响应
//!     let mut stream = provider.complete_stream(request).await?;
//!     
//!     while let Some(result) = stream.next().await {
//!         match result {
//!             Ok(chunk) => {
//!                 if let Some(content) = &chunk.choices[0].delta.content {
//!                     print!("{}", content);
//!                 }
//!             }
//!             Err(e) => eprintln!("Error: {}", e),
//!         }
//!     }
//!     
//!     Ok(())
//! }
//! ```
//!
//! ## 使用自定义 API 端点
//!
//! ```rust
//! use provider::ProviderConfig;
//!
//! let config = ProviderConfig::new("your-api-key")
//!     .with_base_url("https://your-custom-endpoint.com/v1")
//!     .with_model("custom-model")
//!     .with_organization("org-123");
//! ```
//!
//! ## 使用工具调用 (Function Calling)
//!
//! ```rust,no_run
//! use provider::{ProviderConfig, OpenAIProvider, Provider, ChatCompletionRequest, ChatMessage, ToolDefinition};
//! use serde_json::json;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let config = ProviderConfig::new("your-api-key");
//!     let provider = OpenAIProvider::new(config)?;
//!     
//!     // 定义工具
//!     let weather_tool = ToolDefinition::new(
//!         "get_weather",
//!         json!({
//!             "type": "object",
//!             "properties": {
//!                 "city": {"type": "string", "description": "城市名称"}
//!             },
//!             "required": ["city"]
//!         })
//!     ).with_description("获取指定城市的天气信息");
//!     
//!     let request = ChatCompletionRequest::new("gpt-4o", vec![
//!         ChatMessage::user("北京今天天气怎么样？"),
//!     ])
//!     .with_tools(vec![weather_tool]);
//!     
//!     let response = provider.complete(request).await?;
//!     
//!     // 检查是否有工具调用
//!     if let Some(tool_calls) = &response.choices[0].message.tool_calls {
//!         for call in tool_calls {
//!             println!("Tool: {}", call.function.name);
//!             println!("Arguments: {}", call.function.arguments);
//!         }
//!     }
//!     
//!     Ok(())
//! }
//! ```

pub mod models;
pub mod provider;

pub use models::*;
pub use provider::*;

