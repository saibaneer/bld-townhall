//! The thin OpenAI-compatible chat transport (M11, ADR-031).
//!
//! ADR-031 chose a small hand-rolled client over `rig-core`: the agent's whole
//! need is "POST chat messages to an OpenAI-compatible endpoint, read the
//! assistant's text back". [`ChatModel`] is that one operation, behind a trait so
//! the proposer's prompt-and-parse logic is testable with a canned responder and
//! never needs a live model. [`OpenAiChat`] is the real implementation over
//! `reqwest`; it points wherever `AGENT_BASE_URL` says (an Ollama endpoint by
//! default), because model/provider is configuration, not architecture.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Where the proposer's model lives. Read from the environment so a cloud
/// open-weight model, a local `qwen3:4b`, or the from-scratch model is a one-line
/// swap that changes no invariant (ADR-031).
#[derive(Clone, Debug)]
pub struct AgentConfig {
    pub base_url: String,
    pub model: String,
    /// Bearer token, when the endpoint needs one. A local Ollama endpoint (which
    /// proxies its own `:cloud` auth) needs none, so this is usually `None`.
    pub api_key: Option<String>,
}

impl AgentConfig {
    /// From `AGENT_BASE_URL` / `AGENT_MODEL` / `AGENT_API_KEY`, defaulting to a
    /// local Ollama endpoint + `qwen3:4b` (the offline reference).
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Self {
        Self {
            base_url: get("AGENT_BASE_URL")
                .unwrap_or_else(|| "http://localhost:11434/v1".to_owned()),
            model: get("AGENT_MODEL").unwrap_or_else(|| "qwen3:4b".to_owned()),
            api_key: get("AGENT_API_KEY").filter(|value| !value.is_empty()),
        }
    }
}

/// What went wrong reaching the model. A proposer treats ANY of these as "no
/// proposal this turn" — the model failing must never become a boundary call.
#[derive(Debug)]
pub enum ChatError {
    /// The endpoint could not be reached.
    Transport(String),
    /// The endpoint answered with a non-success status.
    Status(u16),
    /// The answer was not the shape an OpenAI-compatible endpoint returns.
    Malformed(String),
}

impl std::fmt::Display for ChatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(detail) => write!(f, "the model could not be reached: {detail}"),
            Self::Status(code) => write!(f, "the model endpoint returned HTTP {code}"),
            Self::Malformed(detail) => write!(f, "the model's answer was unreadable: {detail}"),
        }
    }
}

impl std::error::Error for ChatError {}

/// One turn with a chat model: a system prompt, a user prompt, the assistant's
/// raw text back. The single seam the [`crate::llm::LlmProposer`] depends on.
#[async_trait]
pub trait ChatModel: Send + Sync {
    /// # Errors
    /// [`ChatError`] if the endpoint cannot be reached, refuses, or answers in an
    /// unexpected shape.
    async fn complete(&self, system: &str, user: &str) -> Result<String, ChatError>;
}

/// The real transport: an OpenAI-compatible `POST {base_url}/chat/completions`.
pub struct OpenAiChat {
    http: reqwest::Client,
    config: AgentConfig,
}

impl OpenAiChat {
    #[must_use]
    pub fn new(config: AgentConfig) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .unwrap_or_default(),
            config,
        }
    }
}

#[async_trait]
impl ChatModel for OpenAiChat {
    async fn complete(&self, system: &str, user: &str) -> Result<String, ChatError> {
        let request = ChatRequest {
            model: &self.config.model,
            messages: vec![
                Message {
                    role: "system",
                    content: system,
                },
                Message {
                    role: "user",
                    content: user,
                },
            ],
            stream: false,
            temperature: 0.0,
        };
        let mut builder = self
            .http
            .post(format!("{}/chat/completions", self.config.base_url))
            .json(&request);
        if let Some(key) = &self.config.api_key {
            builder = builder.bearer_auth(key);
        }
        let response = builder
            .send()
            .await
            .map_err(|error| ChatError::Transport(error.to_string()))?;
        if !response.status().is_success() {
            return Err(ChatError::Status(response.status().as_u16()));
        }
        let parsed: ChatResponse = response
            .json()
            .await
            .map_err(|error| ChatError::Malformed(error.to_string()))?;
        parsed
            .choices
            .into_iter()
            .next()
            .map(|choice| choice.message.content)
            .ok_or_else(|| ChatError::Malformed("no choices in the completion".to_owned()))
    }
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<Message<'a>>,
    stream: bool,
    temperature: f32,
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChoiceMessage,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    #[serde(default)]
    content: String,
}
