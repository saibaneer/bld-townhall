#![forbid(unsafe_code)]

//! The demo binary: a scripted SMS conversation against a running
//! townhall-server, through exactly the runner the acceptance test uses.
//!
//! ```text
//! sms-simulator --server http://127.0.0.1:PORT --script scripts/lucy-journey.txt
//! ```
//!
//! # M7C: the real authority lane
//!
//! Approve-first books nothing until a person answers a challenge, and the
//! delegation their `YES` produces is what authorizes the booking. So this
//! composes against the REAL resolver, not the dev lane: one fixed workload
//! credential (`agent-townhall`) authenticates the caller and authorizes
//! nothing, and every change presents a reference an approval issued. The
//! approval and evidence ports reach the server over HTTP (from the shared
//! `townhall-http-ports` crate), exactly as the booking gateway does — the
//! dispatcher names only the traits.

use std::process::ExitCode;
use std::sync::Arc;
use townhall_channel::{ChannelAddress, ChannelConfig, Region, SmsSimulator, SuppressionStore};
use townhall_http_ports::{HttpApprovals, HttpEvidence, HttpUsage, WORKLOAD};
use townhall_orchestrator::{
    ContinuationStore, CredentialSource, Dispatcher, FileContinuation, FileSuppression,
    GatewayFactory, PrincipalDirectory, ScriptedProposer, journey,
};

struct DevDirectory;

impl PrincipalDirectory for DevDirectory {
    fn resolve(&self, address: &ChannelAddress) -> Option<bld_types::PrincipalId> {
        match address.revealed() {
            "+447700900123" => Some(bld_types::PrincipalId::new("lucy")),
            "+447700900456" => Some(bld_types::PrincipalId::new("priya")),
            _ => None,
        }
    }
}

/// The credential swap (ADR-025/026): every recognized principal presents the
/// SAME fixed workload credential. It cannot widen authority even in principle —
/// a change is refused unless a delegation reference an approval produced rides
/// with it.
struct WorkloadCredential;

impl CredentialSource for WorkloadCredential {
    fn token_for(&self, principal: &bld_types::PrincipalId) -> Option<String> {
        matches!(principal.as_str(), "lucy" | "priya").then(|| WORKLOAD.to_owned())
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let (mut server, mut script_path) = (None, None);
    // The durable files default BESIDE the process, not in the SHARED temp dir —
    // the review's point: a predictable path under /tmp is world-guessable and
    // holds phone numbers.
    let mut stop_file = "sms-simulator-stop.list".to_owned();
    let mut continuation_file = "sms-simulator-continuation.jsonl".to_owned();
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--server" => server = args.next(),
            "--script" => script_path = args.next(),
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
                eprintln!("sms-simulator: unknown flag {other:?}");
                return ExitCode::from(2);
            }
        }
    }
    let (Some(server), Some(script_path)) = (server, script_path) else {
        eprintln!("sms-simulator: --server URL --script PATH are both required");
        return ExitCode::from(2);
    };

    let text = match std::fs::read_to_string(&script_path) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("sms-simulator: cannot read {script_path}: {error}");
            return ExitCode::from(2);
        }
    };
    let script = match journey::Script::parse(&text) {
        Ok(script) => script,
        Err(error) => {
            eprintln!("sms-simulator: {error}");
            return ExitCode::from(2);
        }
    };

    let suppression: Arc<dyn SuppressionStore> =
        match FileSuppression::open(std::path::PathBuf::from(stop_file)) {
            Ok(store) => Arc::new(store),
            Err(error) => {
                eprintln!("sms-simulator: suppression store: {error}");
                return ExitCode::from(2);
            }
        };
    let continuations: Arc<dyn ContinuationStore> =
        match FileContinuation::open(std::path::PathBuf::from(continuation_file)) {
            Ok(store) => Arc::new(store),
            Err(error) => {
                eprintln!("sms-simulator: continuation store: {error}");
                return ExitCode::from(2);
            }
        };
    let channel = Arc::new(SmsSimulator::new(
        ChannelConfig::default(),
        Arc::clone(&suppression),
    ));
    let http = reqwest::Client::new();
    let dispatcher = Dispatcher::new(
        Arc::clone(&channel),
        Arc::new(DevDirectory),
        Arc::new(WorkloadCredential),
        Arc::new(HttpUsage::new(server.clone(), http.clone())),
        Arc::new(ScriptedProposer),
        suppression,
        Arc::new(GatewayFactory {
            base: server.clone(),
        }),
        Arc::new(HttpApprovals::new(server.clone(), http.clone())),
        Arc::new(HttpEvidence::new(server, http)),
        continuations,
    );

    // Complete anything a prior crashed run left approved-but-unbooked, before
    // the journey — a booking owed is a booking made (ADR-026, W7).
    dispatcher.resume().await;

    match journey::run(&dispatcher, &channel, &script, Region::Gb).await {
        Ok(()) => {
            // The transcript IS the demo — a demo of an SMS conversation that hid
            // the SMS would demonstrate nothing. The address is masked by its own
            // Debug; the text is fixture conversation from the script, never live
            // user data, and that boundary is what this print relies on.
            for sent in channel.outbox() {
                println!("BLD -> {:?}: {}", sent.to, sent.text);
            }
            println!("journey complete");
            ExitCode::SUCCESS
        }
        Err(divergence) => {
            eprintln!("sms-simulator: {divergence}");
            ExitCode::FAILURE
        }
    }
}
