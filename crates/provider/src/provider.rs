use anyhow::{anyhow, Result};
use futures::Stream;
use reqwest::Client;
use std::pin::Pin;

use crate::models::*;

pub type StreamResult = Pin<Box<dyn Stream<Item = Result<StreamResponse>> + Send>>;

#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;

    async fn complete(&self, request: ChatCompletionRequest) -> Result<ChatCompletionResponse>;

    async fn complete_stream(&self, request: ChatCompletionRequest) -> Result<StreamResult>;
}

#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub organization: Option<String>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            base_url: "https://api.openai.com/v1".to_string(),
            model: "gpt-4o".to_string(),
            organization: None,
        }
    }
}

impl ProviderConfig {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            ..Default::default()
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    pub fn with_organization(mut self, organization: impl Into<String>) -> Self {
        self.organization = Some(organization.into());
        self
    }
}

pub struct OpenAIProvider {
    config: ProviderConfig,
    client: Client,
}

impl OpenAIProvider {
    pub fn new(config: ProviderConfig) -> Result<Self> {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()?;

        Ok(Self { config, client })
    }

    fn build_request(&self, request: &ChatCompletionRequest) -> reqwest::RequestBuilder {
        let url = format!("{}/chat/completions", self.config.base_url);
        let mut builder = self
            .client
            .post(&url)
            .bearer_auth(&self.config.api_key)
            .json(request);

        if let Some(org) = &self.config.organization {
            builder = builder.header("OpenAI-Organization", org);
        }

        builder
    }
}

#[async_trait::async_trait]
impl Provider for OpenAIProvider {
    fn name(&self) -> &str {
        "openai"
    }

    async fn complete(&self, mut request: ChatCompletionRequest) -> Result<ChatCompletionResponse> {
        request.stream = None;

        let response = self.build_request(&request).send().await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await?;
            return Err(anyhow!("API error ({}): {}", status, body));
        }

        let completion = response.json::<ChatCompletionResponse>().await?;
        Ok(completion)
    }

    async fn complete_stream(
        &self,
        mut request: ChatCompletionRequest,
    ) -> Result<StreamResult> {
        use futures::stream::StreamExt;
        use reqwest_eventsource::{Event, EventSource};

        request.stream = Some(true);

        let builder = self.build_request(&request);
        let event_source = EventSource::new(builder)?;

        let stream = event_source
            .take_while(|event| {
                futures::future::ready(!matches!(event, Ok(Event::Message(ref msg)) if msg.data == "[DONE]"))
            })
            .filter_map(|event| async move {
                match event {
                    Ok(Event::Message(msg)) => {
                        let response: Result<StreamResponse> =
                            serde_json::from_str(&msg.data).map_err(Into::into);
                        Some(response)
                    }
                    Ok(Event::Open) => None,
                    Err(e) => Some(Err(anyhow!("Stream error: {}", e))),
                }
            });

        Ok(Box::pin(stream))
    }
}

pub struct ProviderFactory;

impl ProviderFactory {
    pub fn create_openai(config: ProviderConfig) -> Result<Box<dyn Provider>> {
        Ok(Box::new(OpenAIProvider::new(config)?))
    }

    pub fn create_anthropic(_config: ProviderConfig) -> Result<Box<dyn Provider>> {
        Err(anyhow!("Anthropic provider not yet implemented"))
    }

    pub fn create_azure(_config: ProviderConfig) -> Result<Box<dyn Provider>> {
        Err(anyhow!("Azure provider not yet implemented"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use wiremock::matchers::{method, path, header, body_json};

    #[test]
    fn test_provider_config_default() {
        let config = ProviderConfig::default();
        assert_eq!(config.api_key, "");
        assert_eq!(config.base_url, "https://api.openai.com/v1");
        assert_eq!(config.model, "gpt-4o");
        assert!(config.organization.is_none());
    }

    #[test]
    fn test_provider_config_builders() {
        let config = ProviderConfig::new("sk-test")
            .with_base_url("https://custom.api.com/v1")
            .with_model("gpt-3.5-turbo")
            .with_organization("org-123");

        assert_eq!(config.api_key, "sk-test");
        assert_eq!(config.base_url, "https://custom.api.com/v1");
        assert_eq!(config.model, "gpt-3.5-turbo");
        assert_eq!(config.organization, Some("org-123".to_string()));
    }

    #[test]
    fn test_openai_provider_creation() {
        let config = ProviderConfig::new("sk-test");
        let provider = OpenAIProvider::new(config);
        assert!(provider.is_ok());
    }

    #[test]
    fn test_openai_provider_name() {
        let config = ProviderConfig::new("sk-test");
        let provider = OpenAIProvider::new(config).unwrap();
        assert_eq!(provider.name(), "openai");
    }

    #[test]
    fn test_provider_factory_openai() {
        let config = ProviderConfig::new("sk-test");
        let provider = ProviderFactory::create_openai(config);
        assert!(provider.is_ok());
        assert_eq!(provider.unwrap().name(), "openai");
    }

    #[test]
    fn test_provider_factory_anthropic_not_implemented() {
        let config = ProviderConfig::new("sk-test");
        let result = ProviderFactory::create_anthropic(config);
        assert!(result.is_err());
    }

    #[test]
    fn test_provider_factory_azure_not_implemented() {
        let config = ProviderConfig::new("sk-test");
        let result = ProviderFactory::create_azure(config);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_complete_success() {
        let mock_server = MockServer::start().await;

        let response_body = serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 1234567890,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Hello, how can I help?"
                },
                "finish_reason": "stop"
            }]
        });

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("Authorization", "Bearer sk-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&response_body))
            .mount(&mock_server)
            .await;

        let config = ProviderConfig::new("sk-test")
            .with_base_url(&mock_server.uri());
        
        let provider = OpenAIProvider::new(config).unwrap();
        let request = ChatCompletionRequest::new("gpt-4o", vec![ChatMessage::user("Hi")]);
        let response = provider.complete(request).await;

        assert!(response.is_ok());
        let response = response.unwrap();
        assert_eq!(response.id, "chatcmpl-test");
        assert_eq!(response.choices[0].message.content, Some("Hello, how can I help?".to_string()));
    }

    #[tokio::test]
    async fn test_complete_with_organization_header() {
        let mock_server = MockServer::start().await;

        let response_body = serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 1234567890,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }]
        });

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("OpenAI-Organization", "org-123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&response_body))
            .mount(&mock_server)
            .await;

        let config = ProviderConfig::new("sk-test")
            .with_base_url(&mock_server.uri())
            .with_organization("org-123");
        
        let provider = OpenAIProvider::new(config).unwrap();
        let request = ChatCompletionRequest::new("gpt-4o", vec![ChatMessage::user("Hi")]);
        let response = provider.complete(request).await;

        assert!(response.is_ok());
    }

    #[tokio::test]
    async fn test_complete_api_error() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "error": {"message": "Invalid API key"}
            })))
            .mount(&mock_server)
            .await;

        let config = ProviderConfig::new("invalid-key")
            .with_base_url(&mock_server.uri());
        
        let provider = OpenAIProvider::new(config).unwrap();
        let request = ChatCompletionRequest::new("gpt-4o", vec![ChatMessage::user("Hi")]);
        let result = provider.complete(request).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("API error"));
    }

    #[tokio::test]
    async fn test_complete_with_tools() {
        let mock_server = MockServer::start().await;

        let response_body = serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 1234567890,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_123",
                        "type": "function",
                        "function": {
                            "name": "get_weather",
                            "arguments": "{\"city\": \"Beijing\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&response_body))
            .mount(&mock_server)
            .await;

        let config = ProviderConfig::new("sk-test")
            .with_base_url(&mock_server.uri());
        
        let provider = OpenAIProvider::new(config).unwrap();
        
        let tools = vec![ToolDefinition::new("get_weather", serde_json::json!({}))];
        let request = ChatCompletionRequest::new("gpt-4o", vec![ChatMessage::user("What's the weather?")])
            .with_tools(tools);
        
        let response = provider.complete(request).await.unwrap();
        
        let tool_calls = response.choices[0].message.tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls[0].function.name, "get_weather");
    }

    #[tokio::test]
    async fn test_complete_request_body() {
        let mock_server = MockServer::start().await;

        let response_body = serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 1234567890,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }]
        });

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(body_json(serde_json::json!({
                "model": "gpt-4o",
                "messages": [{
                    "role": "user",
                    "content": "Hello"
                }],
                "temperature": 0.5,
                "max_tokens": 100
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(&response_body))
            .mount(&mock_server)
            .await;

        let config = ProviderConfig::new("sk-test")
            .with_base_url(&mock_server.uri());
        
        let provider = OpenAIProvider::new(config).unwrap();
        let request = ChatCompletionRequest::new("gpt-4o", vec![ChatMessage::user("Hello")])
            .with_temperature(0.5)
            .with_max_tokens(100);
        
        let result = provider.complete(request).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    #[ignore]
    async fn test_real_api_call() {
        dotenvy::dotenv().ok();
        let api_key = std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY not set");
        let base_url = std::env::var("OPENAI_BASE_URL").ok();
        let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o".to_string());

        let mut config = ProviderConfig::new(api_key).with_model(&model);
        if let Some(url) = base_url {
            config = config.with_base_url(url);
        }

        let provider = OpenAIProvider::new(config).unwrap();
        let request = ChatCompletionRequest::new(&model, vec![ChatMessage::user("Say hello in one word")]);

        let response = provider.complete(request).await;
        match &response {
            Ok(resp) => {
                assert!(!resp.choices.is_empty());
                println!("Response: {:?}", resp.choices[0].message.content);
            }
            Err(e) => {
                println!("Error: {:?}", e);
            }
        }
        assert!(response.is_ok());
    }
}
