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
//! approval and evidence ports reach the server over HTTP, exactly as the
//! booking gateway does — the dispatcher names only the traits.

use async_trait::async_trait;
use std::process::ExitCode;
use std::sync::Arc;
use townhall_channel::{ChannelAddress, ChannelConfig, Region, SmsSimulator, SuppressionStore};
use townhall_orchestrator::{
    ApprovalError, ApprovalPort, BeginApproval, ContinuationStore, CredentialSource, Deposited,
    Dispatcher, EvidenceDeposit, FileContinuation, FileSuppression, GatewayFactory,
    InboundEvidence, PrincipalDirectory, Raised, ScriptedProposer, UsageDenied, UsageLedger,
    journey,
};

/// The one workload credential the real resolver knows — a WORKLOAD, not a
/// person. It authenticates the caller and authorizes nothing; authority rides
/// the delegation reference an approval issues, never the token.
const WORKLOAD: &str = "agent-townhall";

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

/// Reaches the server's approval endpoints over HTTP, holding the workload
/// credential — the same way the booking gateway reaches `/booking-intents`.
/// Owns its request bodies (this crate cannot name `townhall-http`).
struct HttpApprovals {
    base: String,
    http: reqwest::Client,
}

#[async_trait]
impl ApprovalPort for HttpApprovals {
    async fn begin(&self, request: &BeginApproval) -> Result<Raised, ApprovalError> {
        let response = self
            .http
            .post(format!("{}/approvals", self.base))
            .header("authorization", format!("Bearer {WORKLOAD}"))
            .json(&serde_json::json!({
                "booking": request.booking,
                "grantor": request.grantor,
                "subject": request.subject,
                "binding_principal": request.binding_principal,
                "binding_version": request.binding_version,
                "behaviours": request.behaviours,
                "purpose": request.purpose,
                "requested_date": request.requested_date,
                "from": request.from,
                "to": request.to,
                "attendees": request.attendees,
                "wheelchair_accessible": request.wheelchair_accessible,
                "max_fee_pence": request.max_fee_pence,
            }))
            .send()
            .await
            .map_err(transport)?;
        // 201 created, 200 reused — both carry the challenge and preview.
        if response.status().is_success() {
            let body = json(response).await?;
            Ok(Raised {
                challenge: field(&body, "challenge"),
                preview: field(&body, "preview"),
            })
        } else {
            Err(ApprovalError::Transport(format!(
                "approvals begin: {}",
                response.status()
            )))
        }
    }

    async fn reply(
        &self,
        challenge: &str,
        answer: &str,
        code: &str,
        receipt: &str,
    ) -> Result<Option<String>, ApprovalError> {
        let response = self
            .http
            .post(format!("{}/approvals/{challenge}/reply", self.base))
            .header("authorization", format!("Bearer {WORKLOAD}"))
            .json(&serde_json::json!({ "answer": answer, "code": code, "receipt": receipt }))
            .send()
            .await
            .map_err(transport)?;
        match response.status().as_u16() {
            201 => Ok(Some(field(&json(response).await?, "delegation"))),
            200 => Ok(None), // a recorded decline (NO).
            403 => {
                // Wrong code, or out of attempts — both 403. The tries-left count
                // rides in the server's message; parse it, defaulting to 0
                // (exhausted) when there is none.
                let text = response.text().await.unwrap_or_default();
                Err(ApprovalError::WrongCode {
                    tries_left: first_number(&text),
                })
            }
            404 | 410 => Err(ApprovalError::Gone(
                response.text().await.unwrap_or_default(),
            )),
            other => Err(ApprovalError::Transport(format!("reply: {other}"))),
        }
    }

    async fn revoke_via_receipt(&self, evidence: &InboundEvidence) -> Result<u32, ApprovalError> {
        let response = self
            .http
            .post(format!("{}/revocations", self.base))
            .header("authorization", format!("Bearer {WORKLOAD}"))
            .json(&serde_json::json!({
                "provider": evidence.provider,
                "account": evidence.account,
                "message_id": evidence.message_id,
                "address": evidence.address,
                "verified": evidence.verified,
                "signature": evidence.signature,
            }))
            .send()
            .await
            .map_err(transport)?;
        match response.status().as_u16() {
            200 => Ok(count(&json(response).await?, "revoked")),
            // Unbound or forged sender (WrongChannel): the server recorded the
            // distinct 403 denial and swept nothing, so the victim's grants are
            // untouched. The SMS surface collapses it to "0 stopped" — a texter
            // must not be able to learn which numbers are bound by watching the
            // reply's wording (ADR-022's anti-enumeration reasoning).
            403 => Ok(0),
            other => Err(ApprovalError::Transport(format!("revoke: {other}"))),
        }
    }
}

/// Deposits an inbound reply's evidence at the ingress endpoint, holding the
/// workload credential, and returns the challenge + receipt.
struct HttpEvidence {
    base: String,
    http: reqwest::Client,
}

#[async_trait]
impl EvidenceDeposit for HttpEvidence {
    async fn deposit(&self, evidence: &InboundEvidence) -> Result<Deposited, ApprovalError> {
        let response = self
            .http
            .post(format!("{}/inbound-evidence", self.base))
            .header("authorization", format!("Bearer {WORKLOAD}"))
            .json(&serde_json::json!({
                "provider": evidence.provider,
                "account": evidence.account,
                "message_id": evidence.message_id,
                "address": evidence.address,
                "verified": evidence.verified,
                "signature": evidence.signature,
            }))
            .send()
            .await
            .map_err(transport)?;
        match response.status().as_u16() {
            code if (200..300).contains(&code) => {
                let body = json(response).await?;
                Ok(Deposited {
                    challenge: field(&body, "challenge"),
                    receipt: field(&body, "receipt"),
                })
            }
            // The number is awaiting no challenge — a reply to nothing.
            404 | 410 => Err(ApprovalError::Gone(
                response.text().await.unwrap_or_default(),
            )),
            other => Err(ApprovalError::Transport(format!("deposit: {other}"))),
        }
    }
}

/// The usage meter over HTTP (M8, ADR-027). Sends only the transport-evidence
/// body; the server derives the principal, the intent and the unit cost.
struct HttpUsage {
    base: String,
    http: reqwest::Client,
}

impl HttpUsage {
    /// The transport-evidence body every `/usage/*` call carries — the same
    /// triple `/inbound-evidence` and `/revocations` take.
    fn body(evidence: &InboundEvidence) -> serde_json::Value {
        serde_json::json!({
            "provider": evidence.provider,
            "account": evidence.account,
            "message_id": evidence.message_id,
            "address": evidence.address,
            "verified": evidence.verified,
            "signature": evidence.signature,
        })
    }

    async fn post(
        &self,
        path: &str,
        evidence: &InboundEvidence,
    ) -> Result<reqwest::Response, String> {
        self.http
            .post(format!("{}{path}", self.base))
            .header("authorization", format!("Bearer {WORKLOAD}"))
            .json(&Self::body(evidence))
            .send()
            .await
            .map_err(|error| error.to_string())
    }
}

#[async_trait]
impl UsageLedger for HttpUsage {
    async fn reserve(&self, evidence: &InboundEvidence) -> Result<(), UsageDenied> {
        let response = self
            .post("/usage/reserve", evidence)
            .await
            .map_err(UsageDenied::Transport)?;
        match response.status().as_u16() {
            200 => Ok(()),
            // 429: the quota is spent. A retry does not help until units free up.
            429 => Err(UsageDenied::QuotaExhausted),
            other => Err(UsageDenied::Transport(format!("reserve: {other}"))),
        }
    }

    async fn debit(&self, evidence: &InboundEvidence) {
        // Best-effort: a lost debit is recovered by the ledger's idempotency on a
        // redelivery; there is nothing to do on a transport failure here.
        let _ = self.post("/usage/debit", evidence).await;
    }

    async fn release(&self, evidence: &InboundEvidence) {
        let _ = self.post("/usage/release", evidence).await;
    }

    async fn describe_balance(&self, evidence: &InboundEvidence) -> String {
        match self.post("/usage/balance", evidence).await {
            Ok(response) if response.status().is_success() => {
                let body: serde_json::Value = response.json().await.unwrap_or_default();
                let remaining = body["remaining"].as_i64().unwrap_or(0);
                let limit = body["limit"].as_i64().unwrap_or(0);
                format!(
                    "You have {remaining} of {limit} usage units left. This command costs nothing."
                )
            }
            _ => "Balance unavailable right now. This command costs nothing.".to_owned(),
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
fn transport(error: reqwest::Error) -> ApprovalError {
    ApprovalError::Transport(error.to_string())
}

async fn json(response: reqwest::Response) -> Result<serde_json::Value, ApprovalError> {
    response.json().await.map_err(transport)
}

fn field(body: &serde_json::Value, key: &str) -> String {
    body[key].as_str().unwrap_or_default().to_owned()
}

/// A non-negative integer field, read as a `u32` — the revoke count. `0` when
/// the key is absent or not a number, which reads as "nothing stopped".
fn count(body: &serde_json::Value, key: &str) -> u32 {
    u32::try_from(body[key].as_u64().unwrap_or(0)).unwrap_or(u32::MAX)
}

/// The first run of ASCII digits in `text`, as a `u8` — how many tries remain,
/// read out of the server's denial message. `0` when there is none.
fn first_number(text: &str) -> u8 {
    let digits: String = text
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().unwrap_or(0)
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
        Arc::new(HttpUsage {
            base: server.clone(),
            http: http.clone(),
        }),
        Arc::new(ScriptedProposer),
        suppression,
        Arc::new(GatewayFactory {
            base: server.clone(),
        }),
        Arc::new(HttpApprovals {
            base: server.clone(),
            http: http.clone(),
        }),
        Arc::new(HttpEvidence { base: server, http }),
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
