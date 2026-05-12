//! Configuration loading module.

use std::env;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("OPENAI_API_KEY not found")]
    ApiKeyNotFound,
    #[error("OPENAI_BASE_URL not found")]
    BaseUrlNotFound,
    #[error("OPENAI_MODEL not found")]
    ModelNotFound,
}

pub struct Config {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
}

pub fn load() -> Result<Config, ConfigError> {
    dotenv::dotenv().ok();

    let api_key = env::var("OPENAI_API_KEY").map_err(|_| ConfigError::ApiKeyNotFound)?;
    let base_url = env::var("OPENAI_BASE_URL").map_err(|_| ConfigError::BaseUrlNotFound)?;
    let model = env::var("OPENAI_MODEL").map_err(|_| ConfigError::ModelNotFound)?;

    Ok(Config {
        api_key,
        base_url,
        model,
    })
}
