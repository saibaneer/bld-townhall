#![forbid(unsafe_code)]

//! A one-shot channel-binding utility (M12 demo support).
//!
//! The approve-first flow authorizes a booking only for a principal already BOUND
//! to the channel address the request arrives on. Binding is a store write with
//! no HTTP endpoint (the same `bind_channel` the authority-lane tests use), so a
//! live demo needs a way to establish it. This writes exactly one binding:
//!
//! ```text
//! bind-channel --db townhall.sqlite --address tg:5741534028 --principal lucy
//! ```
//!
//! It opens the SAME database the running townhall-server opened; SQLite's WAL
//! mode lets this brief write proceed alongside the server's connection.

use std::process::ExitCode;

use townhall_authority::AssuranceLevel;
use townhall_store::authority::{ChannelBinding, SqlApprovalStore};

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or(u64::MAX)
}

#[tokio::main]
async fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let (mut db, mut address, mut principal) = (None, None, None);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--db" => db = args.next(),
            "--address" => address = args.next(),
            "--principal" => principal = args.next(),
            other => {
                eprintln!("bind-channel: unknown flag {other:?}");
                return ExitCode::from(2);
            }
        }
    }
    let (Some(db), Some(address), Some(principal)) = (db, address, principal) else {
        eprintln!("bind-channel: --db PATH --address ADDR --principal NAME are all required");
        return ExitCode::from(2);
    };

    let repository = match townhall_store::SqliteBookingRepository::open(&db).await {
        Ok(repository) => repository,
        Err(error) => {
            eprintln!("bind-channel: cannot open {db}: {error}");
            return ExitCode::FAILURE;
        }
    };
    let store = SqlApprovalStore::new(repository.pool().clone());

    // Assurance `SmsReply` is the level a real channel-reply binding establishes —
    // the person proved control of the channel by replying on it (here, by
    // messaging the bot). It is the level the approve-first flow expects.
    let binding = ChannelBinding {
        id: format!("binding-{principal}-{address}"),
        address: address.clone(),
        principal: bld_types::PrincipalId::new(&principal),
        version: 1,
        assurance: AssuranceLevel::SmsReply,
        withdrawn: false,
    };
    match store
        .bind_channel(&binding, Some("m12 demo bind"), now_ms())
        .await
    {
        Ok(()) => {
            println!("bind-channel: bound {address} -> {principal} (assurance sms-reply)");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("bind-channel: bind failed: {error}");
            ExitCode::FAILURE
        }
    }
}
