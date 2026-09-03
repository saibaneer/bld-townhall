//! Shared test doubles for the approve-first ports, so the unit tests exercise
//! the dispatcher's real control flow without a server.

#![allow(dead_code)] // each test file uses a different subset

use async_trait::async_trait;
use bld_types::{BookingId, PrincipalId};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use townhall_orchestrator::{
    ApprovalError, ApprovalPort, BeginApproval, Continuation, ContinuationStore, Deposited,
    EvidenceDeposit, InboundEvidence, Raised,
};

/// The in-memory analogue of `FileContinuation` — same load/record/clear/resume
/// contract, backed by a `Vec`, so a unit test can seed a mid-flow state and read
/// the result back.
#[derive(Default)]
pub struct MemoryContinuation {
    held: Mutex<Vec<Continuation>>,
}

impl MemoryContinuation {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Plant a continuation, for a test that begins mid-conversation.
    pub fn seed(&self, continuation: Continuation) {
        self.locked().push(continuation);
    }

    /// Every held continuation, for readback assertions.
    #[must_use]
    pub fn all(&self) -> Vec<Continuation> {
        self.locked().clone()
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, Vec<Continuation>> {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl ContinuationStore for MemoryContinuation {
    fn load(&self, principal: &PrincipalId) -> Option<Continuation> {
        self.locked()
            .iter()
            .rev()
            .find(|held| held.principal.as_str() == principal.as_str())
            .cloned()
    }

    fn load_for_booking(&self, booking: &BookingId) -> Option<Continuation> {
        self.locked()
            .iter()
            .rev()
            .find(|held| held.booking_id.as_str() == booking.as_str())
            .cloned()
    }

    fn record(&self, continuation: Continuation) -> Result<(), String> {
        let mut held = self.locked();
        held.retain(|held| held.booking_id.as_str() != continuation.booking_id.as_str());
        held.push(continuation);
        Ok(())
    }

    fn clear(&self, challenge_id: &str) -> Result<(), String> {
        self.locked()
            .retain(|held| held.challenge_id != challenge_id);
        Ok(())
    }

    fn take_resumable(&self) -> Vec<Continuation> {
        self.locked()
            .iter()
            .filter(|held| held.reference.is_some())
            .cloned()
            .collect()
    }
}

/// A stub approval port: `begin` returns a fixed challenge and a preview carrying
/// a known code; `reply` returns a fixed reference. Enough for the tests that
/// never actually approve (control/grammar ordering).
#[derive(Default)]
pub struct StubApprovals {
    pub begins: AtomicUsize,
    pub replies: AtomicUsize,
}

#[async_trait]
impl ApprovalPort for StubApprovals {
    async fn begin(&self, request: &BeginApproval) -> Result<Raised, ApprovalError> {
        self.begins.fetch_add(1, Ordering::SeqCst);
        Ok(Raised {
            challenge: format!("ch-{}", request.booking),
            preview: "Reply YES 0000 to approve. Maximum booking fee: £50.00.".to_owned(),
        })
    }

    async fn reply(
        &self,
        challenge: &str,
        _answer: &str,
        _code: &str,
        _receipt: &str,
    ) -> Result<Option<String>, ApprovalError> {
        self.replies.fetch_add(1, Ordering::SeqCst);
        Ok(Some(format!("ref-{challenge}")))
    }
}

/// A stub deposit port: returns a fixed challenge + receipt for any inbound.
#[derive(Default)]
pub struct StubEvidence {
    pub deposits: AtomicUsize,
}

#[async_trait]
impl EvidenceDeposit for StubEvidence {
    async fn deposit(&self, evidence: &InboundEvidence) -> Result<Deposited, ApprovalError> {
        self.deposits.fetch_add(1, Ordering::SeqCst);
        Ok(Deposited {
            challenge: format!("ch-{}", evidence.message_id),
            receipt: "receipt".to_owned(),
        })
    }
}

const WORKLOAD: &str = "agent-townhall";

/// The REAL approval port over HTTP — the same shape the sms-simulator wires,
/// for integration tests that drive the dispatcher against a live server. Owns
/// its request bodies (this crate cannot name `townhall-http`).
pub struct HttpApprovals {
    pub base: String,
    pub http: reqwest::Client,
}

impl HttpApprovals {
    #[must_use]
    pub fn new(base: &str) -> Self {
        Self {
            base: base.to_owned(),
            http: reqwest::Client::new(),
        }
    }
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
        if response.status().is_success() {
            let body = read_json(response).await?;
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
            201 => Ok(Some(field(&read_json(response).await?, "delegation"))),
            200 => Ok(None),
            403 => {
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
}

/// The REAL deposit port over HTTP.
pub struct HttpEvidence {
    pub base: String,
    pub http: reqwest::Client,
}

impl HttpEvidence {
    #[must_use]
    pub fn new(base: &str) -> Self {
        Self {
            base: base.to_owned(),
            http: reqwest::Client::new(),
        }
    }
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
                let body = read_json(response).await?;
                Ok(Deposited {
                    challenge: field(&body, "challenge"),
                    receipt: field(&body, "receipt"),
                })
            }
            404 | 410 => Err(ApprovalError::Gone(
                response.text().await.unwrap_or_default(),
            )),
            other => Err(ApprovalError::Transport(format!("deposit: {other}"))),
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
fn transport(error: reqwest::Error) -> ApprovalError {
    ApprovalError::Transport(error.to_string())
}

async fn read_json(response: reqwest::Response) -> Result<serde_json::Value, ApprovalError> {
    response.json().await.map_err(transport)
}

fn field(body: &serde_json::Value, key: &str) -> String {
    body[key].as_str().unwrap_or_default().to_owned()
}

fn first_number(text: &str) -> u8 {
    text.chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap_or(0)
}
