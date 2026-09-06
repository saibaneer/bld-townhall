#![forbid(unsafe_code)]

//! The Telegram composition root (M12 / ADR-033): the live counterpart of
//! `sms-simulator`.
//!
//! ```text
//! TELEGRAM_BOT_TOKEN=… telegram-runner --server http://127.0.0.1:PORT --lucy-chat 5741534028
//! ```
//!
//! It wires the Telegram [`TelegramChannel`] and the dispatcher against a running
//! `townhall-server`, then **long-polls** the Bot API: each inbound message
//! becomes a `RawInbound` the dispatcher handles, the dispatcher's replies go back
//! out over the same channel, and the update offset advances so no message is
//! processed twice (belt-and-braces with the channel's own replay window). The
//! same fixed workload credential and HTTP ports `sms-simulator` uses apply here —
//! the dispatcher names only the traits.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use telegram_client::{TelegramClient, TelegramConfig};
use townhall_channel::{
    ChannelAddress, ChannelKind, InboundIdentity, RawInbound, SuppressionStore, TransportEvidence,
};
use townhall_http_ports::{HttpApprovals, HttpEvidence, HttpUsage, WORKLOAD};
use townhall_orchestrator::{
    ContinuationStore, CredentialSource, Dispatcher, FileContinuation, FileSuppression,
    GatewayFactory, PrincipalDirectory, ScriptedProposer,
};
use townhall_telegram_channel::{TelegramChannel, telegram_channel_config};

/// Maps a single Telegram chat to the demo principal `lucy`. A real deployment
/// would resolve many chats against a durable directory; for the demo one chat is
/// bound to one person, and every other chat is a stranger the boundary refuses.
struct TelegramDirectory {
    lucy_chat: i64,
}

impl PrincipalDirectory for TelegramDirectory {
    fn resolve(&self, address: &ChannelAddress) -> Option<bld_types::PrincipalId> {
        match address.telegram_chat_id() {
            Some(id) if id == self.lucy_chat => Some(bld_types::PrincipalId::new("lucy")),
            _ => None,
        }
    }
}

/// The credential swap (ADR-025/026): the recognized principal presents the SAME
/// fixed workload credential, which authorizes nothing on its own.
struct WorkloadCredential;

impl CredentialSource for WorkloadCredential {
    fn token_for(&self, principal: &bld_types::PrincipalId) -> Option<String> {
        (principal.as_str() == "lucy").then(|| WORKLOAD.to_owned())
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(i64::MAX)
}

#[tokio::main]
async fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let (mut server, mut lucy_chat) = (None, None);
    let mut stop_file = "telegram-runner-stop.list".to_owned();
    let mut continuation_file = "telegram-runner-continuation.jsonl".to_owned();
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--server" => server = args.next(),
            "--lucy-chat" => lucy_chat = args.next().and_then(|v| v.parse::<i64>().ok()),
            "--stop-file" => {
                if let Some(path) = args.next() {
                    stop_file = path;
                }
            }
            "--continuation-file" => {
                if let Some(path) = args.next() {
                    continuation_file = path;
                }
            }
            other => {
                eprintln!("telegram-runner: unknown flag {other:?}");
                return ExitCode::from(2);
            }
        }
    }
    let (Some(server), Some(lucy_chat)) = (server, lucy_chat) else {
        eprintln!(
            "telegram-runner: --server URL --lucy-chat CHAT_ID are both required \
             (and TELEGRAM_BOT_TOKEN in the environment)"
        );
        return ExitCode::from(2);
    };

    let config = match TelegramConfig::from_env(|name| std::env::var(name).ok()) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("telegram-runner: {error}");
            return ExitCode::from(2);
        }
    };

    let suppression: Arc<dyn SuppressionStore> =
        match FileSuppression::open(std::path::PathBuf::from(stop_file)) {
            Ok(store) => Arc::new(store),
            Err(error) => {
                eprintln!("telegram-runner: suppression store: {error}");
                return ExitCode::from(2);
            }
        };
    let continuations: Arc<dyn ContinuationStore> =
        match FileContinuation::open(std::path::PathBuf::from(continuation_file)) {
            Ok(store) => Arc::new(store),
            Err(error) => {
                eprintln!("telegram-runner: continuation store: {error}");
                return ExitCode::from(2);
            }
        };

    let http = reqwest::Client::new();
    let client = Arc::new(TelegramClient::new(http.clone(), config));
    let channel = Arc::new(TelegramChannel::new(
        Arc::clone(&client),
        telegram_channel_config(),
        Arc::clone(&suppression),
    ));
    let dispatcher = Dispatcher::new(
        Arc::clone(&channel),
        Arc::new(TelegramDirectory { lucy_chat }),
        Arc::new(WorkloadCredential),
        Arc::new(HttpUsage::new(server.clone(), http.clone())),
        Arc::new(ScriptedProposer),
        suppression,
        Arc::new(GatewayFactory {
            base: server.clone(),
        }),
        Arc::new(HttpApprovals::new(server.clone(), http.clone())),
        Arc::new(HttpEvidence::new(server.clone(), http)),
        continuations,
    );

    // NOTE: `resume()` is intentionally NOT called here. It reparses stored
    // continuation addresses as phone numbers (Region::Gb), which a `tg:` address
    // is not — crash-resume for the Telegram address form is a follow-up. A fresh
    // run has nothing to resume regardless.

    eprintln!(
        "telegram-runner: listening (server={server}, lucy_chat={lucy_chat}). \
         Text the bot, e.g. `BOOK date=2026-09-10 from=14:00 to=17:00 people=20 accessible=yes max=5000`."
    );
    poll_forever(dispatcher, client).await
}

/// The long-poll loop: drain the pre-launch backlog (so an earlier `/start` is
/// not read as a booking), then feed each new inbound message to the dispatcher
/// and deliver the convergence follow-ups it owes. Never returns.
async fn poll_forever(dispatcher: Dispatcher<TelegramChannel>, client: Arc<TelegramClient>) -> ! {
    // Drain the pre-launch backlog, RETRYING until it succeeds: a failed drain
    // would leave the offset unset and reprocess the person's earlier messages out
    // of context (a transient TLS blip on the first request must not do that).
    let mut offset = loop {
        match client.get_updates(None).await {
            Ok(updates) => break updates.iter().map(|u| u.update_id + 1).max(),
            Err(error) => {
                eprintln!("telegram-runner: initial get_updates (retrying): {error}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    };
    loop {
        match client.get_updates(offset).await {
            Ok(updates) => {
                for update in updates {
                    // Advance past every update, even ones we skip, so a
                    // non-message update never wedges the offset.
                    offset = Some(update.update_id + 1);
                    let (Some(chat_id), Some(body)) = (update.chat_id, update.text) else {
                        continue;
                    };
                    let raw = RawInbound {
                        identity: InboundIdentity::new(
                            "telegram",
                            "bot",
                            update.update_id.to_string(),
                        ),
                        channel: ChannelKind::Telegram,
                        from: chat_id.to_string(),
                        body,
                        received_at_ms: now_ms(),
                        evidence: TransportEvidence::new("telegram", chat_id.to_string(), true),
                    };
                    if let Err(error) = dispatcher.handle(raw).await {
                        eprintln!("telegram-runner: handle: {error:?}");
                    }
                }
            }
            Err(error) => {
                eprintln!("telegram-runner: get_updates: {error}");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
        // Deliver any convergence follow-ups the dispatcher now owes, then breathe
        // before the next poll (short-poll; a real deployment would long-poll).
        dispatcher.run_followups().await;
        tokio::time::sleep(Duration::from_millis(1000)).await;
    }
}
