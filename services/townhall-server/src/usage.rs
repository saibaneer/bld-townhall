//! The server's usage meter: it turns a transport-evidence body into a metered
//! (or refused) turn, deriving everything load-bearing itself (ADR-027).
//!
//! The dispatcher sends only the inbound triple + address. This resolves the
//! sender to a live binding for the principal, derives the `UsageIntentId` from
//! the triple, and lets the `UsageService` price the turn — so a compromised
//! dispatcher can meter only turns it can present transport evidence for, and
//! never a victim's account or a caller-chosen unit count.

use crate::authority::now_ms;
use bld_types::{PrincipalId, UsageIntentId};
use std::sync::Arc;
use townhall_authority::ApprovalStore; // for `live_binding_by_address`
use townhall_channel::InboundIdentity;
use townhall_http::approvals::InboundEvidence;
use townhall_http::usage::{UsageBalanceView, UsageMeter, UsageMeterError};
use townhall_store::authority::SqlApprovalStore;
use townhall_store::usage::SqlUsageStore;
use townhall_usage::{UsageDenied, UsageService};

pub struct ServiceMeter {
    pub usage: Arc<UsageService<SqlUsageStore>>,
    /// The binding resolver — the SAME rows the approval plane resolves a sender
    /// against, so "who is this turn for" has one answer across both planes.
    pub bindings: Arc<SqlApprovalStore>,
}

impl ServiceMeter {
    /// Resolve the sender to the principal its live binding names — the server's
    /// answer to "whose account", never the caller's.
    async fn principal_of(&self, address: &str) -> Option<PrincipalId> {
        self.bindings
            .live_binding_by_address(address)
            .await
            .ok()
            .flatten()
            .map(|bound| bound.reference.principal)
    }

    /// Derive the intent from the transport triple — the caller cannot forge it.
    fn intent(inbound: &InboundEvidence) -> UsageIntentId {
        InboundIdentity::new(&inbound.provider, &inbound.account, &inbound.message_id)
            .usage_intent_id()
    }

    /// The channel-rate key: the provider account, from the transport triple —
    /// the caller does not name it (M8-2).
    fn channel(inbound: &InboundEvidence) -> String {
        format!("{}|{}", inbound.provider, inbound.account)
    }
}

#[async_trait::async_trait]
impl UsageMeter for ServiceMeter {
    async fn reserve(&self, inbound: &InboundEvidence) -> Result<(), UsageMeterError> {
        let Some(principal) = self.principal_of(&inbound.address).await else {
            // The sender resolves to no live binding. The dispatcher only reaches
            // the meter for a directory-known sender, so this is a demo-consistency
            // gap between the directory and channel_bindings, not a reason to trap
            // the person behind a turn they cannot run — meter nothing, allow it.
            // (M8-2, which owns global limits, tightens this.)
            return Ok(());
        };
        self.usage
            .reserve(
                &principal,
                &Self::intent(inbound),
                &Self::channel(inbound),
                now_ms(),
            )
            .await
            .map_err(map_denied)
    }

    async fn debit(&self, inbound: &InboundEvidence) {
        if self.principal_of(&inbound.address).await.is_some() {
            let _ = self.usage.debit(&Self::intent(inbound), now_ms()).await;
        }
    }

    async fn release(&self, inbound: &InboundEvidence) {
        if self.principal_of(&inbound.address).await.is_some() {
            let _ = self.usage.release(&Self::intent(inbound), now_ms()).await;
        }
    }

    async fn balance(
        &self,
        inbound: &InboundEvidence,
    ) -> Result<UsageBalanceView, UsageMeterError> {
        let Some(principal) = self.principal_of(&inbound.address).await else {
            // An unbound number reports the default allowance unused — a zero-unit
            // read that reveals no binding status.
            let limit = self.usage.policy().default_limit_units;
            return Ok(UsageBalanceView {
                remaining: limit,
                limit,
            });
        };
        let balance = self.usage.balance(&principal).await.map_err(map_denied)?;
        Ok(UsageBalanceView {
            remaining: balance.remaining(),
            limit: balance.limit_units,
        })
    }
}

fn map_denied(error: UsageDenied) -> UsageMeterError {
    match error {
        UsageDenied::QuotaExhausted => UsageMeterError::QuotaExhausted,
        UsageDenied::PrincipalRateLimited => UsageMeterError::PrincipalRateLimited,
        UsageDenied::ChannelRateLimited => UsageMeterError::ChannelRateLimited,
        UsageDenied::ProviderBudgetExhausted => UsageMeterError::ProviderBudgetExhausted,
        UsageDenied::Unavailable(why) => UsageMeterError::Unavailable(why),
    }
}
