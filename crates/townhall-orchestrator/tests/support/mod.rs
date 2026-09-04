//! Shared test doubles for the approve-first ports, so the unit tests exercise
//! the dispatcher's real control flow without a server.

#![allow(dead_code)] // each test file uses a different subset

use async_trait::async_trait;
use bld_types::{BookingId, PrincipalId};
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use townhall_orchestrator::{
    ApprovalError, ApprovalPort, BeginApproval, Continuation, ContinuationStore, Deposited,
    EvidenceDeposit, InboundEvidence, Raised, UsageDenied, UsageLedger,
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
///
/// `revoke_via_receipt` is deliberately NOT a hardcoded `Ok(0)`: it returns a
/// count derived from grants a test seeds per sender address, and zeroes them —
/// so a REVOKE witness fails a dispatcher that never calls the port, mis-parses
/// the count, or is not idempotent (the never-fake-tests rule).
#[derive(Default)]
pub struct StubApprovals {
    pub begins: AtomicUsize,
    pub replies: AtomicUsize,
    pub revocations: AtomicUsize,
    /// Sender address -> live grants the next REVOKE from it will stop.
    grants: Mutex<HashMap<String, u32>>,
}

impl StubApprovals {
    /// Seed `n` live grants for `address`, the count its next REVOKE returns.
    pub fn seed_grants(&self, address: &str, n: u32) {
        self.grants
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(address.to_owned(), n);
    }
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

    async fn revoke_via_receipt(&self, evidence: &InboundEvidence) -> Result<u32, ApprovalError> {
        self.revocations.fetch_add(1, Ordering::SeqCst);
        // Return the seeded count for this sender, then zero it — a replayed
        // REVOKE stops nothing more (idempotent), exactly as the real sweep.
        let mut grants = self
            .grants
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(grants.insert(evidence.address.clone(), 0).unwrap_or(0))
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
            200 => Ok(count(&read_json(response).await?, "revoked")),
            // Unbound/forged sender: the server swept nothing and recorded the
            // 403; the surface collapses it to "0 stopped" (anti-enumeration).
            403 => Ok(0),
            other => Err(ApprovalError::Transport(format!("revoke: {other}"))),
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

/// The REAL usage meter over HTTP — the same shape the sms-simulator wires, so
/// the conversation tests exercise real metering against a live server.
pub struct HttpUsage {
    pub base: String,
    pub http: reqwest::Client,
}

impl HttpUsage {
    #[must_use]
    pub fn new(base: &str) -> Self {
        Self {
            base: base.to_owned(),
            http: reqwest::Client::new(),
        }
    }

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
            429 => Err(UsageDenied::QuotaExhausted),
            other => Err(UsageDenied::Transport(format!("reserve: {other}"))),
        }
    }
    async fn debit(&self, evidence: &InboundEvidence) {
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

async fn read_json(response: reqwest::Response) -> Result<serde_json::Value, ApprovalError> {
    response.json().await.map_err(transport)
}

fn field(body: &serde_json::Value, key: &str) -> String {
    body[key].as_str().unwrap_or_default().to_owned()
}

/// A non-negative integer field as a `u32` — the revoke count.
fn count(body: &serde_json::Value, key: &str) -> u32 {
    u32::try_from(body[key].as_u64().unwrap_or(0)).unwrap_or(u32::MAX)
}

fn first_number(text: &str) -> u8 {
    text.chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap_or(0)
}
