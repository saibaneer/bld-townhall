#![forbid(unsafe_code)]

//! The orchestrator's approval / evidence / usage ports, over HTTP.
//!
//! Each reaches a running `townhall-server` holding the one fixed workload
//! credential — the same way the booking gateway reaches `/booking-intents`. The
//! dispatcher names only the traits (`ApprovalPort`, `EvidenceDeposit`,
//! `UsageLedger`); these are the concrete adapters a composition root plugs in.
//! Owns its own request bodies, because none of the composition roots can name
//! `townhall-http`.

use async_trait::async_trait;
use townhall_orchestrator::{
    ApprovalError, ApprovalPort, BeginApproval, Deposited, EvidenceDeposit, InboundEvidence,
    Raised, UsageDenied, UsageLedger,
};

/// The one workload credential the real resolver knows — a WORKLOAD, not a
/// person. It authenticates the caller and authorizes nothing; authority rides
/// the delegation reference an approval issues, never the token.
pub const WORKLOAD: &str = "agent-townhall";

/// Reaches the server's approval endpoints over HTTP, holding the workload
/// credential — the same way the booking gateway reaches `/booking-intents`.
pub struct HttpApprovals {
    base: String,
    http: reqwest::Client,
}

impl HttpApprovals {
    #[must_use]
    pub fn new(base: String, http: reqwest::Client) -> Self {
        Self { base, http }
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
            .json(&evidence_body(evidence))
            .send()
            .await
            .map_err(transport)?;
        match response.status().as_u16() {
            200 => Ok(count(&json(response).await?, "revoked")),
            // Unbound or forged sender (WrongChannel): the server recorded the
            // distinct 403 denial and swept nothing. The SMS surface collapses it
            // to "0 stopped" — a texter must not be able to learn which numbers are
            // bound by watching the reply's wording (ADR-022's anti-enumeration).
            403 => Ok(0),
            other => Err(ApprovalError::Transport(format!("revoke: {other}"))),
        }
    }
}

/// Deposits an inbound reply's evidence at the ingress endpoint, holding the
/// workload credential, and returns the challenge + receipt.
pub struct HttpEvidence {
    base: String,
    http: reqwest::Client,
}

impl HttpEvidence {
    #[must_use]
    pub fn new(base: String, http: reqwest::Client) -> Self {
        Self { base, http }
    }
}

#[async_trait]
impl EvidenceDeposit for HttpEvidence {
    async fn deposit(&self, evidence: &InboundEvidence) -> Result<Deposited, ApprovalError> {
        let response = self
            .http
            .post(format!("{}/inbound-evidence", self.base))
            .header("authorization", format!("Bearer {WORKLOAD}"))
            .json(&evidence_body(evidence))
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
pub struct HttpUsage {
    base: String,
    http: reqwest::Client,
}

impl HttpUsage {
    #[must_use]
    pub fn new(base: String, http: reqwest::Client) -> Self {
        Self { base, http }
    }

    async fn post(
        &self,
        path: &str,
        evidence: &InboundEvidence,
    ) -> Result<reqwest::Response, String> {
        self.http
            .post(format!("{}{path}", self.base))
            .header("authorization", format!("Bearer {WORKLOAD}"))
            .json(&evidence_body(evidence))
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
            // Every resource denial is 429; the body's `denial_code` says which.
            // A missing or unparseable body degrades to QuotaExhausted — still a
            // refusal for a resource reason, never an allow.
            429 => {
                let body: serde_json::Value = response.json().await.unwrap_or_default();
                Err(usage_denied_from_code(body["denial_code"].as_str()))
            }
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

/// The transport-evidence triple every ingress/usage/revoke call carries.
fn evidence_body(evidence: &InboundEvidence) -> serde_json::Value {
    serde_json::json!({
        "provider": evidence.provider,
        "account": evidence.account,
        "message_id": evidence.message_id,
        "address": evidence.address,
        "verified": evidence.verified,
        "signature": evidence.signature,
    })
}

/// Map a 429's `denial_code` body to the usage denial it names. An absent or
/// unrecognized code falls back to `QuotaExhausted` — still a resource refusal,
/// never an allow (ADR-028).
fn usage_denied_from_code(code: Option<&str>) -> UsageDenied {
    match code {
        Some("rate_limited_principal") => UsageDenied::PrincipalRateLimited,
        Some("rate_limited_channel") => UsageDenied::ChannelRateLimited,
        Some("provider_budget_exhausted") => UsageDenied::ProviderBudgetExhausted,
        _ => UsageDenied::QuotaExhausted,
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

/// A non-negative integer field, read as a `u32` — the revoke count. `0` when the
/// key is absent or not a number, which reads as "nothing stopped".
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
