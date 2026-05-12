//! Application assembly and lifecycle management.

use crate::agent::Agent;
use crate::bus::Bus;
use crate::config;
use crate::model::{Model, OpenAIProvider};
use crate::session::Session;
use crate::tools::ToolRegistry;
use crate::tui;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("config error: {0}")]
    Config(#[from] config::ConfigError),
    #[error("model error: {0}")]
    Model(#[from] crate::model::ModelError),
    #[error("terminal error: {0}")]
    Terminal(#[from] Box<dyn std::error::Error>),
}

pub async fn run() -> Result<(), AppError> {
    let cfg = config::load()?;

    let provider = OpenAIProvider::new(cfg.api_key, Some(cfg.base_url))?;
    let model = Model::new(Box::new(provider), &cfg.model)?;
    let session = Session::new("You are a helpful coding assistant.", 100_000);
    let registry = ToolRegistry::with_default_tools();

    let bus = Bus::new(256);
    let agent = Agent::new(model, session, registry, bus, 20);
    let model_name = cfg.model.clone();
    let handle = agent.spawn(16);

    tui::run_tui(handle, model_name).await.map_err(AppError::Terminal)
}
