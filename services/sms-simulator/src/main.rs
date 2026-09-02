#![forbid(unsafe_code)]

//! The demo binary: a scripted SMS conversation against a running
//! townhall-server, through exactly the runner the acceptance test uses.
//!
//! ```text
//! sms-simulator --server http://127.0.0.1:PORT --script scripts/lucy-journey.txt
//! ```
//!
//! The dev bindings here mirror the server's dev-authority allowlist: this
//! binary is a composition root for a DEMO, and it composes the same parts the
//! tests exercise — which is the point of it.

use std::process::ExitCode;
use std::sync::Arc;
use townhall_channel::{ChannelAddress, ChannelConfig, Region, SmsSimulator, SuppressionStore};
use townhall_orchestrator::{
    CredentialSource, Dispatcher, FileSuppression, GatewayFactory, NoLedgerYet, PrincipalDirectory,
    ScriptedProposer, journey,
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

struct DevCredentials;

impl CredentialSource for DevCredentials {
    fn token_for(&self, principal: &bld_types::PrincipalId) -> Option<String> {
        match principal.as_str() {
            "lucy" => Some("dev-lucy".to_owned()),
            "priya" => Some("dev-priya-nobook".to_owned()),
            _ => None,
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let (mut server, mut script_path) = (None, None);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--server" => server = args.next(),
            "--script" => script_path = args.next(),
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
        match FileSuppression::open(std::env::temp_dir().join("sms-simulator-stop.list")) {
            Ok(store) => Arc::new(store),
            Err(error) => {
                eprintln!("sms-simulator: suppression store: {error}");
                return ExitCode::from(2);
            }
        };
    let channel = Arc::new(SmsSimulator::new(
        ChannelConfig::default(),
        Arc::clone(&suppression),
    ));
    let dispatcher = Dispatcher::new(
        Arc::clone(&channel),
        Arc::new(DevDirectory),
        Arc::new(DevCredentials),
        Arc::new(NoLedgerYet),
        Arc::new(ScriptedProposer),
        suppression,
        Arc::new(GatewayFactory { base: server }),
    );

    match journey::run(&dispatcher, &channel, &script, Region::Gb).await {
        Ok(()) => {
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
