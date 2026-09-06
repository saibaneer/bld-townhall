#![cfg(feature = "telegram-live")]

//! The `telegram-live` lane (M12, ADR-033): the SAME adapter, driven against the
//! REAL Bot API at `api.telegram.org` rather than the hermetic mock.
//!
//! It proves the one thing the mock cannot: our request encoding and response
//! parsing match real Telegram. It finds the chat that has messaged the bot
//! (`get_updates`) and sends one real message to it — so the person running it
//! sees it arrive on their phone.
//!
//! OPT-IN, never part of a normal `cargo test`: it needs a real token and a user
//! who has sent the bot `/start`, plus network.
//!
//! ```text
//! set -a; source .env; set +a
//! cargo test -p telegram-client --features telegram-live -- --nocapture
//! ```

use telegram_client::{TelegramClient, TelegramConfig};

/// Missing config means FAIL LOUDLY, never a silent skip.
fn client() -> TelegramClient {
    let config = TelegramConfig::from_env(|name| std::env::var(name).ok())
        .expect("the telegram-live lane needs TELEGRAM_BOT_TOKEN in the environment");
    TelegramClient::new(reqwest::Client::new(), config)
}

#[tokio::test]
async fn a_real_message_sends_and_returns_a_message_id() {
    let client = client();

    // Find the chat that has messaged the bot. TELEGRAM_TEST_CHAT_ID pins it
    // explicitly; otherwise use the most recent update's chat.
    let chat_id: i64 = if let Ok(id) = std::env::var("TELEGRAM_TEST_CHAT_ID") {
        id.parse()
            .expect("TELEGRAM_TEST_CHAT_ID must be an integer")
    } else {
        let updates = client.get_updates(None).await.expect("get_updates");
        updates
            .iter()
            .rev()
            .find_map(|u| u.chat_id)
            .expect("no chat has messaged the bot yet — send it /start from your phone first")
    };
    eprintln!("telegram-live: sending to chat_id {chat_id}");

    let sent = client
        .send_message(
            chat_id,
            "BLD boundary — M12 live Telegram adapter is up. (telegram-live lane)",
        )
        .await
        .expect("a real send to a chat that has started the bot succeeds");

    eprintln!(
        "telegram-live: sent message_id {} to chat {}",
        sent.message_id, sent.chat_id
    );
    assert!(
        sent.message_id > 0,
        "a real Telegram message id is positive"
    );
    assert_eq!(sent.chat_id, chat_id, "it landed in the target chat");
}
