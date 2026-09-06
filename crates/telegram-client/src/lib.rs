#![forbid(unsafe_code)]

//! Telegram Bot API over HTTP — a real two-way human channel for M12.
//!
//! A trusted transport adapter in the spirit of `twilio-client`: it carries a
//! canonical outbound reply to Telegram and returns the provider's raw
//! observation (the message id). It mints no domain fact and asserts no identity
//! — that authority stays in the channel/domain layers.
//!
//! Why Telegram at all (ADR-033): Twilio's SMS/WhatsApp path to a real phone runs
//! a gauntlet of telecom compliance (numbers, regulatory bundles, KYC, per-country
//! routing). Telegram's Bot API needs none of it — a bot token from `@BotFather`,
//! plain HTTPS, and **inbound by long-polling** ([`Self::get_updates`]) so there is
//! no public webhook or tunnel to stand up. The BLD boundary is channel-agnostic,
//! so proving it over Telegram proves exactly what SMS would, minus the paperwork.
//!
//! Two operations, both Telegram's protocol, not ours:
//! - [`TelegramClient::send_message`] — `POST …/bot<token>/sendMessage`.
//! - [`TelegramClient::get_updates`] — `GET …/bot<token>/getUpdates` (long-poll).
//!
//! The bot token authenticates by sitting in the URL PATH (Telegram's design), so
//! it is held behind a type whose `Debug` never reveals it, and transport errors
//! are stripped of their URL so the token cannot leak into a log.

use std::fmt;

use serde::Deserialize;
use thiserror::Error;

/// A Telegram bot token whose `Debug` never reveals the secret. Whoever holds it
/// controls the bot, so it is the one secret this crate guards.
#[derive(Clone)]
pub struct TelegramBotToken(String);

impl TelegramBotToken {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The raw token, for the one place it must cross a boundary: the request
    /// path. Named to make its use conspicuous at call sites.
    #[must_use]
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for TelegramBotToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TelegramBotToken(****)")
    }
}

/// Everything the adapter needs told to it, never assumed.
#[derive(Clone, Debug)]
pub struct TelegramConfig {
    /// The bot token from `@BotFather` (`<digits>:<base64ish>`).
    pub bot_token: TelegramBotToken,
}

impl TelegramConfig {
    /// Read the config from a getter (usually `|n| std::env::var(n).ok()`).
    ///
    /// # Errors
    /// [`TelegramError::Config`] when `TELEGRAM_BOT_TOKEN` is unset or empty — a
    /// blank token must never silently "work".
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Result<Self, TelegramError> {
        let token = get("TELEGRAM_BOT_TOKEN")
            .filter(|value| !value.trim().is_empty())
            .ok_or(TelegramError::Config("TELEGRAM_BOT_TOKEN"))?;
        Ok(Self {
            bot_token: TelegramBotToken::new(token),
        })
    }
}

/// The provider's raw observation of an accepted send — never a domain fact.
#[derive(Clone, Debug)]
pub struct SentMessage {
    /// Telegram's message id within the chat.
    pub message_id: i64,
    /// The chat the message landed in — echoes the target, useful for logging.
    pub chat_id: i64,
}

/// One inbound update, flattened from Telegram's nested shape to what the channel
/// layer needs: who it is from (the chat to reply to) and what they said.
#[derive(Clone, Debug)]
pub struct Update {
    /// Monotonic update id — the next `get_updates` passes `update_id + 1` as
    /// `offset` to acknowledge it and not receive it again.
    pub update_id: i64,
    /// The chat id to reply to (a private chat's id is the user's). `None` for an
    /// update that carries no message (edited/service updates).
    pub chat_id: Option<i64>,
    /// The message text, if any.
    pub text: Option<String>,
    /// The sender's @username, if they have one.
    pub from_username: Option<String>,
}

/// Everything the Bot API can answer that is not a success.
#[derive(Debug, Error)]
pub enum TelegramError {
    /// A required config value is unset or empty.
    #[error("{0} is unset")]
    Config(&'static str),
    /// The HTTP call itself failed. The URL — which carries the token — is
    /// stripped from the message so the secret cannot leak into a log.
    #[error("telegram transport error: {0}")]
    Transport(String),
    /// The Bot API answered `ok: false` — carries Telegram's own description
    /// (e.g. "chat not found", "bot was blocked by the user"). No secret in it.
    #[error("telegram API error: {0}")]
    Api(String),
    /// A body that did not parse as a Bot API response.
    #[error("unrecognized telegram response: {0}")]
    BadResponse(String),
}

#[derive(Deserialize)]
struct SendResult {
    message_id: i64,
    chat: ChatRef,
}

#[derive(Deserialize)]
struct ChatRef {
    id: i64,
}

#[derive(Deserialize)]
struct RawUpdate {
    update_id: i64,
    #[serde(default)]
    message: Option<RawMessage>,
}

#[derive(Deserialize)]
struct RawMessage {
    chat: ChatRef,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    from: Option<RawFrom>,
}

#[derive(Deserialize)]
struct RawFrom {
    #[serde(default)]
    username: Option<String>,
}

/// Talks to one Telegram Bot API.
pub struct TelegramClient {
    http: reqwest::Client,
    base_url: String,
    token: TelegramBotToken,
}

impl TelegramClient {
    #[must_use]
    pub fn new(http: reqwest::Client, config: TelegramConfig) -> Self {
        Self {
            http,
            base_url: "https://api.telegram.org".to_owned(),
            token: config.bot_token,
        }
    }

    /// Point the client at a different base URL — for a hermetic mock in tests.
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Send one message to `chat_id`. Returns the provider's raw observation.
    ///
    /// # Errors
    /// [`TelegramError::Transport`], [`TelegramError::Api`] (Telegram's own
    /// `ok:false` reason), [`TelegramError::BadResponse`].
    pub async fn send_message(
        &self,
        chat_id: i64,
        text: &str,
    ) -> Result<SentMessage, TelegramError> {
        let result: SendResult = self
            .call(
                "sendMessage",
                Some(serde_json::json!({ "chat_id": chat_id, "text": text })),
            )
            .await?;
        Ok(SentMessage {
            message_id: result.message_id,
            chat_id: result.chat.id,
        })
    }

    /// Long-poll for inbound updates. `offset` acknowledges everything before it
    /// (pass the last seen `update_id + 1`); `None` returns the backlog.
    ///
    /// This is the whole inbound path — no webhook, no public URL, no signature
    /// to verify, because the bot pulls rather than being pushed to.
    ///
    /// # Errors
    /// [`TelegramError::Transport`], [`TelegramError::Api`], [`TelegramError::BadResponse`].
    pub async fn get_updates(&self, offset: Option<i64>) -> Result<Vec<Update>, TelegramError> {
        let body = offset.map(|o| serde_json::json!({ "offset": o }));
        let raw: Vec<RawUpdate> = self.call("getUpdates", body).await?;
        Ok(raw
            .into_iter()
            .map(|u| Update {
                update_id: u.update_id,
                chat_id: u.message.as_ref().map(|m| m.chat.id),
                text: u.message.as_ref().and_then(|m| m.text.clone()),
                from_username: u
                    .message
                    .as_ref()
                    .and_then(|m| m.from.as_ref())
                    .and_then(|f| f.username.clone()),
            })
            .collect())
    }

    /// One Bot API method call: `POST …/bot<token>/<method>` with an optional JSON
    /// body, unwrapping the `{ok, result, description}` envelope every response
    /// wears. Navigated as a `Value` so the envelope stays non-generic.
    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        body: Option<serde_json::Value>,
    ) -> Result<T, TelegramError> {
        let url = format!(
            "{}/bot{}/{method}",
            self.base_url,
            self.token.expose_secret()
        );
        let mut request = self.http.post(&url);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(|error| {
            // Strip the URL (which carries the token) before it reaches a message.
            TelegramError::Transport(error.without_url().to_string())
        })?;
        let text = response.text().await.unwrap_or_default();
        let envelope: serde_json::Value = serde_json::from_str(&text)
            .map_err(|error| TelegramError::BadResponse(error.to_string()))?;
        if envelope.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
            let description = envelope
                .get("description")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("ok=false")
                .to_owned();
            return Err(TelegramError::Api(description));
        }
        let result = envelope
            .get("result")
            .ok_or_else(|| TelegramError::BadResponse("ok=true but no result".to_owned()))?;
        serde_json::from_value(result.clone())
            .map_err(|error| TelegramError::BadResponse(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_reads_the_token_or_names_it_missing() {
        let cfg =
            TelegramConfig::from_env(|n| (n == "TELEGRAM_BOT_TOKEN").then(|| "123:abc".to_owned()))
                .expect("present");
        assert_eq!(cfg.bot_token.expose_secret(), "123:abc");
        // Missing.
        assert!(matches!(
            TelegramConfig::from_env(|_| None),
            Err(TelegramError::Config("TELEGRAM_BOT_TOKEN"))
        ));
        // Blank is treated as unset.
        assert!(matches!(
            TelegramConfig::from_env(|_| Some("   ".to_owned())),
            Err(TelegramError::Config("TELEGRAM_BOT_TOKEN"))
        ));
    }

    #[test]
    fn the_token_never_prints_itself() {
        let token = TelegramBotToken::new("123456:SECRET-VALUE");
        assert_eq!(format!("{token:?}"), "TelegramBotToken(****)");
        let cfg = TelegramConfig { bot_token: token };
        assert!(!format!("{cfg:?}").contains("SECRET-VALUE"));
    }

    /// The mock's capture: the last request's (path, body).
    type Captured = std::sync::Arc<std::sync::Mutex<(String, String)>>;

    struct Mock {
        base: String,
        captured: Captured,
    }

    /// A oneshot mock of the Bot API that records the path (which carries the
    /// token and method) and body, and answers whatever `responder` returns.
    async fn mock(responder: serde_json::Value) -> Mock {
        use std::sync::{Arc, Mutex};

        use axum::extract::State;
        use axum::http::Uri;
        use axum::routing::post;
        use axum::{Json, Router};

        let captured: Captured = Arc::new(Mutex::new((String::new(), String::new())));
        let state = (Arc::clone(&captured), responder);
        let app =
            Router::new().fallback(post(
                |State((cap, resp)): State<(Captured, serde_json::Value)>,
                 uri: Uri,
                 body: String| async move {
                    *cap.lock().expect("lock") = (uri.path().to_owned(), body);
                    Json(resp)
                },
            ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, app.with_state(state))
                .await
                .expect("serve");
        });
        Mock {
            base: format!("http://{addr}"),
            captured,
        }
    }

    fn client(base: &str) -> TelegramClient {
        TelegramClient::new(
            reqwest::Client::new(),
            TelegramConfig {
                bot_token: TelegramBotToken::new("BOTTOKEN123"),
            },
        )
        .with_base_url(base)
    }

    #[tokio::test]
    async fn send_message_puts_the_token_in_the_path_and_the_fields_in_the_body() {
        let m = mock(serde_json::json!({
            "ok": true,
            "result": {"message_id": 42, "chat": {"id": 12345}}
        }))
        .await;
        let sent = client(&m.base)
            .send_message(12345, "hello from the boundary")
            .await
            .expect("the mock accepts the send");
        assert_eq!(sent.message_id, 42);
        assert_eq!(sent.chat_id, 12345);

        let (path, body) = m.captured.lock().expect("lock").clone();
        assert_eq!(
            path, "/botBOTTOKEN123/sendMessage",
            "the token authenticates by sitting in the path, then the method"
        );
        let json: serde_json::Value = serde_json::from_str(&body).expect("json body");
        assert_eq!(json["chat_id"], 12345);
        assert_eq!(json["text"], "hello from the boundary");
    }

    #[tokio::test]
    async fn an_ok_false_response_becomes_a_named_api_error() {
        let m = mock(serde_json::json!({
            "ok": false,
            "error_code": 403,
            "description": "Forbidden: bot was blocked by the user"
        }))
        .await;
        match client(&m.base).send_message(1, "x").await {
            Err(TelegramError::Api(desc)) => {
                assert!(desc.contains("blocked by the user"), "{desc}");
            }
            other => panic!("ok:false must surface as Api error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_updates_flattens_chat_id_text_and_sender() {
        let m = mock(serde_json::json!({
            "ok": true,
            "result": [
                {"update_id": 100, "message": {
                    "message_id": 5,
                    "chat": {"id": 999, "type": "private", "first_name": "Toba"},
                    "from": {"id": 999, "username": "toba"},
                    "text": "/start"
                }},
                {"update_id": 101}
            ]
        }))
        .await;
        let updates = client(&m.base)
            .get_updates(Some(100))
            .await
            .expect("updates");
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].update_id, 100);
        assert_eq!(updates[0].chat_id, Some(999));
        assert_eq!(updates[0].text.as_deref(), Some("/start"));
        assert_eq!(updates[0].from_username.as_deref(), Some("toba"));
        // An update with no message flattens to None, not an error.
        assert_eq!(updates[1].chat_id, None);

        // The offset was sent, so a caller can acknowledge and not re-receive.
        let (path, body) = m.captured.lock().expect("lock").clone();
        assert_eq!(path, "/botBOTTOKEN123/getUpdates");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).expect("json")["offset"],
            100
        );
    }
}
