#![forbid(unsafe_code)]

//! The Telegram [`HumanChannel`] — M12's real human edge (ADR-033).
//!
//! It implements the SAME `townhall-channel` trait the in-process `SmsSimulator`
//! does, so the orchestrator's dispatcher is unchanged: `receive` normalizes an
//! inbound Telegram update into an [`InboundMessage`] (bounding the body and
//! deduping on message identity), and `send` puts an outbound reply on the wire
//! via [`telegram_client::TelegramClient::send_message`], honouring suppression.
//!
//! The address is a Telegram chat id, carried as [`ChannelAddress::telegram`]
//! (`tg:<id>`) — named for what it is, never disguised as a phone number. This
//! crate lives apart from `townhall-channel` because that crate excludes `reqwest`
//! by design; the provider dependency belongs here.

use std::sync::Arc;

use async_trait::async_trait;
use telegram_client::TelegramClient;
use townhall_channel::{
    ChannelAddress, ChannelConfig, ChannelError, HumanChannel, InboundBody, InboundMessage,
    MessageReceipt, OutboundClass, OutboundMessage, RawInbound, ReplayWindow, Seen,
    SuppressionStore,
};

/// A real human channel backed by the Telegram Bot API.
pub struct TelegramChannel {
    client: Arc<TelegramClient>,
    config: ChannelConfig,
    window: ReplayWindow,
    suppression: Arc<dyn SuppressionStore>,
}

impl TelegramChannel {
    /// # Panics
    /// On a configuration [`ChannelConfig::validated`] refuses.
    #[must_use]
    pub fn new(
        client: Arc<TelegramClient>,
        config: ChannelConfig,
        suppression: Arc<dyn SuppressionStore>,
    ) -> Self {
        let config = config.validated().expect("a satisfiable channel config");
        Self {
            client,
            window: ReplayWindow::new(config.replay_window_ms),
            config,
            suppression,
        }
    }

    /// Wall-clock milliseconds — a real channel keeps real time (the replay window
    /// is bounded in ms). Unlike the simulator's steppable clock, this is live.
    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|d| i64::try_from(d.as_millis()).ok())
            .unwrap_or(i64::MAX)
    }
}

#[async_trait]
impl HumanChannel for TelegramChannel {
    type Address = ChannelAddress;

    async fn receive(&self, raw: RawInbound) -> Result<InboundMessage, ChannelError> {
        // Same order as the simulator: normalize the address, bound the body,
        // THEN dedupe — so a malformed flood cannot evict real replay entries.
        // The Telegram address is the chat id; `raw.from` carries it as a string.
        let chat_id: i64 = raw
            .from
            .parse()
            .map_err(|_| ChannelError::UnroutableAddress(ChannelAddress::mask_raw(&raw.from)))?;
        let address = ChannelAddress::telegram(chat_id);
        let body = InboundBody::parse(&raw.body)?;

        if self.window.insert_if_absent(&raw.identity, Self::now_ms()) == Seen::Duplicate {
            return Err(ChannelError::Duplicate);
        }

        Ok(InboundMessage {
            identity: raw.identity,
            channel: raw.channel,
            address,
            received_at_ms: raw.received_at_ms,
            body,
            transport_evidence: raw.evidence,
        })
    }

    async fn send(
        &self,
        to: &Self::Address,
        message: OutboundMessage,
    ) -> Result<MessageReceipt, ChannelError> {
        // Suppression is honoured HERE, in the send path, so no caller can route
        // around it — the same discipline the simulator keeps.
        if message.class == OutboundClass::Automated && self.suppression.is_suppressed(to) {
            return Ok(MessageReceipt::Suppressed);
        }
        // A non-Telegram address reaching the Telegram channel is a routing bug,
        // not a delivery outcome — surface it as unroutable.
        let Some(chat_id) = to.telegram_chat_id() else {
            return Err(ChannelError::UnroutableAddress(ChannelAddress::mask_raw(
                to.revealed(),
            )));
        };

        // Telegram messages don't segment like SMS, but they DO have a hard length
        // limit; bound the text to the configured ceiling rather than let the API
        // reject an over-long message. Truncation is reported, never silent.
        let ceiling = usize::from(self.config.segment_ceiling);
        let (text, truncated) = if message.text.chars().count() > ceiling {
            (message.text.chars().take(ceiling).collect(), true)
        } else {
            (message.text.clone(), false)
        };

        // A delivery failure is an OUTCOME (`Failed`), never an `Err` — the same
        // contract the trait documents.
        match self.client.send_message(chat_id, &text).await {
            Ok(_) => Ok(MessageReceipt::Delivered {
                segments: 1,
                truncated,
            }),
            Err(error) => Ok(MessageReceipt::Failed {
                reason: error.to_string(),
            }),
        }
    }
}

/// The channel config a Telegram deployment wants: no SMS segment ceiling worth
/// speaking of (Telegram allows 4096-char messages), but the same replay window.
/// Region is irrelevant — a Telegram address never goes through the phone parser.
#[must_use]
pub fn telegram_channel_config() -> ChannelConfig {
    ChannelConfig {
        segment_ceiling: 4096,
        ..ChannelConfig::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use telegram_client::{TelegramBotToken, TelegramConfig};
    use townhall_channel::{ChannelKind, InboundIdentity, TransportEvidence};

    fn suppression() -> Arc<townhall_channel::simulator::InMemorySuppression> {
        Arc::new(townhall_channel::simulator::InMemorySuppression::default())
    }

    fn raw(update_id: &str, from: &str, body: &str) -> RawInbound {
        RawInbound {
            identity: InboundIdentity::new("telegram", "bot", update_id),
            channel: ChannelKind::Telegram,
            from: from.to_owned(),
            body: body.to_owned(),
            received_at_ms: 0,
            evidence: TransportEvidence::new("telegram", from, true),
        }
    }

    fn channel(client: TelegramClient, sup: Arc<dyn SuppressionStore>) -> TelegramChannel {
        TelegramChannel::new(Arc::new(client), telegram_channel_config(), sup)
    }

    fn offline_client() -> TelegramClient {
        // A client whose base URL points nowhere — receive() never calls it, and
        // the send tests below use a mock instead.
        TelegramClient::new(
            reqwest::Client::new(),
            TelegramConfig {
                bot_token: TelegramBotToken::new("TESTTOKEN"),
            },
        )
    }

    #[tokio::test]
    async fn receive_turns_a_chat_id_into_a_telegram_address() {
        let ch = channel(offline_client(), suppression());
        let msg = ch
            .receive(raw("100", "5741534028", "BOOK date=2026-09-10"))
            .await
            .expect("routable");
        assert_eq!(msg.address.telegram_chat_id(), Some(5_741_534_028));
        assert_eq!(msg.address.revealed(), "tg:5741534028");
        assert_eq!(msg.body.revealed(), "BOOK date=2026-09-10");
        assert_eq!(msg.channel, ChannelKind::Telegram);
    }

    #[tokio::test]
    async fn receive_dedupes_on_identity() {
        let ch = channel(offline_client(), suppression());
        ch.receive(raw("100", "999", "hi")).await.expect("first");
        // Same identity (same update id) → Duplicate, even with different text.
        assert!(matches!(
            ch.receive(raw("100", "999", "hi again")).await,
            Err(ChannelError::Duplicate)
        ));
        // A different update id is accepted.
        assert!(ch.receive(raw("101", "999", "hi")).await.is_ok());
    }

    #[tokio::test]
    async fn receive_refuses_a_non_numeric_from() {
        let ch = channel(offline_client(), suppression());
        assert!(matches!(
            ch.receive(raw("100", "not-a-chat-id", "hi")).await,
            Err(ChannelError::UnroutableAddress(_))
        ));
    }

    // ---- send, against an in-process mock of the Bot API ----

    type Captured = std::sync::Arc<std::sync::Mutex<Option<serde_json::Value>>>;

    async fn mock_bot() -> (String, Captured) {
        use axum::extract::State;
        use axum::routing::post;
        use axum::{Json, Router};

        let captured: Captured = std::sync::Arc::new(std::sync::Mutex::new(None));
        let app = Router::new().fallback(post(
            |State(cap): State<Captured>, body: String| async move {
                *cap.lock().expect("lock") = serde_json::from_str(&body).ok();
                Json(
                    serde_json::json!({"ok": true, "result": {"message_id": 7, "chat": {"id": 1}}}),
                )
            },
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let state = captured.clone();
        tokio::spawn(async move {
            axum::serve(listener, app.with_state(state))
                .await
                .expect("serve");
        });
        (format!("http://{addr}"), captured)
    }

    #[tokio::test]
    async fn send_delivers_to_the_chat_id_over_the_bot_api() {
        let (base, captured) = mock_bot().await;
        let ch = channel(offline_client().with_base_url(base), suppression());

        let receipt = ch
            .send(
                &ChannelAddress::telegram(5_741_534_028),
                OutboundMessage::reply("Booked. Council ref TH-42."),
            )
            .await
            .expect("send is infallible; delivery is an outcome");
        assert!(matches!(
            receipt,
            MessageReceipt::Delivered {
                segments: 1,
                truncated: false
            }
        ));
        let body = captured.lock().expect("lock").clone().expect("a request");
        assert_eq!(body["chat_id"], 5_741_534_028_i64);
        assert_eq!(body["text"], "Booked. Council ref TH-42.");
    }

    #[tokio::test]
    async fn an_automated_send_to_a_suppressed_address_is_withheld_without_calling_out() {
        let (base, captured) = mock_bot().await;
        let sup = suppression();
        let addr = ChannelAddress::telegram(999);
        sup.suppress(&addr).expect("suppress");
        let ch = channel(offline_client().with_base_url(base), sup);

        let receipt = ch
            .send(
                &addr,
                OutboundMessage::automated("a follow-up you asked to stop"),
            )
            .await
            .expect("send");
        assert!(matches!(receipt, MessageReceipt::Suppressed));
        assert!(
            captured.lock().expect("lock").is_none(),
            "a suppressed automated message must not reach the wire at all"
        );
    }

    #[tokio::test]
    async fn a_reply_ignores_suppression_the_way_the_trait_says() {
        // STOP silences AUTOMATED messages only; a direct Reply still goes.
        let (base, captured) = mock_bot().await;
        let sup = suppression();
        let addr = ChannelAddress::telegram(999);
        sup.suppress(&addr).expect("suppress");
        let ch = channel(offline_client().with_base_url(base), sup);

        let receipt = ch
            .send(&addr, OutboundMessage::reply("your STATUS"))
            .await
            .expect("send");
        assert!(matches!(receipt, MessageReceipt::Delivered { .. }));
        assert!(captured.lock().expect("lock").is_some());
    }

    #[tokio::test]
    async fn a_phone_address_reaching_the_telegram_channel_is_unroutable() {
        let ch = channel(offline_client(), suppression());
        let phone =
            ChannelAddress::parse("+447700900123", townhall_channel::Region::Gb).expect("a phone");
        assert!(matches!(
            ch.send(&phone, OutboundMessage::reply("x")).await,
            Err(ChannelError::UnroutableAddress(_))
        ));
    }
}
