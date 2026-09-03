//! The composition root's authority: real codes, real references, real
//! delegations read back out of the database.
//!
//! # What separates this from `DevAuthority`
//!
//! The dev lane mints a grant per request and hands it straight back — nobody
//! is asked, and nothing is ever read again. This resolver does the opposite:
//! it can mint nothing. A grant exists because somebody answered a challenge
//! through the endpoints, and `resolve_delegation` looks it up, checks it is
//! live and checks it belongs to the caller.
//!
//! ADR-025's amendment to ADR-021 requires exactly this arrangement: the real
//! resolver is the DEFAULT, and `--dev-authority` selects the stand-in
//! explicitly. Without the flag a feature-enabled build resolves through here —
//! never through a silent dev fallback.

use bld_types::{ActorId, DelegationId, PrincipalId};
use std::sync::Arc;
use townhall_authority::{ApprovalCode, AuthorityService, Entropy, service::ResolveError};
use townhall_domain::VerifiedAuthority;
use townhall_http::AuthorityResolver;
use townhall_store::authority::SqlApprovalStore;

/// Codes and identifiers from the operating system.
///
/// # Why `/dev/urandom` and not a crate
///
/// The workspace has no RNG dependency, and adding one for two calls in a
/// composition root is a poor trade. `/dev/urandom` is the kernel's CSPRNG, it
/// never blocks after boot, and reading it is the same operation every RNG
/// crate performs underneath.
pub struct OsEntropy;

impl OsEntropy {
    /// Fill `bytes` from the kernel, or die.
    ///
    /// A panic rather than a fallback: a server that cannot obtain randomness
    /// must not invent a code. The alternative — a predictable code — is worse
    /// than no service, because it looks like a service.
    fn fill(bytes: &mut [u8]) {
        use std::io::Read as _;
        std::fs::File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(bytes))
            .expect("the kernel's CSPRNG is readable");
    }
}

impl Entropy for OsEntropy {
    /// A uniform four-digit code.
    ///
    /// # Why rejection sampling rather than `% 10_000`
    ///
    /// `u32 % 10_000` is biased: 2^32 is not a multiple of 10,000, so the
    /// lowest 4,496 codes are very slightly likelier. The bias is tiny and the
    /// fix is four lines, and the reason to take the four lines is that this is
    /// the number bounding a brute force. Writing a knowingly-skewed sampler
    /// here would be a habit to copy somewhere it matters more.
    fn code(&self) -> ApprovalCode {
        // The largest multiple of 10,000 that fits in a u32; anything at or
        // above it is discarded rather than folded.
        const LIMIT: u32 = u32::MAX - (u32::MAX % 10_000);
        loop {
            let mut bytes = [0u8; 4];
            Self::fill(&mut bytes);
            let drawn = u32::from_le_bytes(bytes);
            if drawn < LIMIT {
                return ApprovalCode::new(format!("{:04}", drawn % 10_000))
                    .expect("four digits, zero padded");
            }
        }
    }

    /// A 256-bit identifier, hex.
    ///
    /// Unguessable matters here more than anywhere: a `DelegationId` is the
    /// reference a caller presents as its grant, so a predictable one is a
    /// bearer token anybody can compute (spec §9.1: "never derived from prompt
    /// text").
    fn identifier(&self) -> String {
        use std::fmt::Write as _;
        let mut bytes = [0u8; 32];
        Self::fill(&mut bytes);
        let mut out = String::with_capacity(64);
        for byte in bytes {
            let _ = write!(out, "{byte:02x}");
        }
        out
    }
}

/// The real resolver.
pub struct RealAuthority {
    service: Arc<AuthorityService<SqlApprovalStore, OsEntropy>>,
    store: Arc<SqlApprovalStore>,
    /// Which bearer belongs to which actor.
    ///
    /// # Why this is still a fixed allowlist
    ///
    /// Authenticating a WORKLOAD credential needs a credential store, and the
    /// POC has none — spec §5's "agent/service authentication as required by
    /// the POC" is deliberately thin. What matters is that it is now a separate
    /// question from authorization: this map answers only "which actor", and
    /// every grant is looked up and checked against that actor afterwards.
    ///
    /// A real deployment replaces this map and nothing else.
    actors: Vec<(String, ActorId)>,
}

impl RealAuthority {
    #[must_use]
    pub fn new(
        service: Arc<AuthorityService<SqlApprovalStore, OsEntropy>>,
        store: Arc<SqlApprovalStore>,
        actors: Vec<(String, ActorId)>,
    ) -> Self {
        Self {
            service,
            store,
            actors,
        }
    }

    /// Block on `future` from a synchronous trait method.
    ///
    /// The resolver trait is synchronous because it is called from inside
    /// Axum's handlers, and the store is not. A thread with its own runtime
    /// rather than `block_in_place`, which panics outright on a current-thread
    /// runtime — the mistake M7A's first SQL implementation made.
    fn block_on<F>(future: F) -> F::Output
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a resolver runtime")
                .block_on(future)
        })
        .join()
        .expect("the resolver thread did not panic")
    }
}

impl AuthorityResolver for RealAuthority {
    fn authenticate(&self, bearer: &str) -> Option<ActorId> {
        self.actors
            .iter()
            .find(|(token, _)| token == bearer)
            // Deliberately NOT a pattern match on a prefix: an allowlist that
            // derives an actor from the shape of a token makes "unknown
            // bearer" impossible, and ADR-021 recorded that as the reason the
            // dev tokens are fixed too.
            .map(|(_, actor)| actor.clone())
    }

    fn resolve_delegation(&self, reference: &str, actor: &ActorId) -> Option<VerifiedAuthority> {
        let service = Arc::clone(&self.service);
        let id = DelegationId::new(reference);
        let now = now_ms();
        let grant = match Self::block_on(async move { service.resolve(&id, now).await }) {
            Ok(grant) => grant,
            // Unknown, revoked, expired and unreadable all answer the same to
            // the caller — the HTTP layer maps `None` to one 401. Kept as
            // distinct arms here so a future audit hook has something to say.
            Err(
                ResolveError::Unknown
                | ResolveError::Revoked
                | ResolveError::Expired
                | ResolveError::Unreadable
                | ResolveError::Unavailable(_),
            ) => return None,
        };

        // The reference alone is not a bearer token.
        //
        // Without this, a delegation id that leaked — into a log, a screenshot,
        // an error report — would be usable by anything that found it. The
        // grant names the actor it was issued to, and only that actor may
        // present it.
        (grant.actor() == actor).then_some(grant)
    }

    fn may_read_for(&self, actor: &ActorId, principal: &PrincipalId) -> bool {
        // An actor may read for a principal whose channel is BOUND.
        //
        // # What this checks, and what it does not
        //
        // It checks the principal is a real, currently-bound person rather than
        // a name the caller typed. It does NOT check that this particular actor
        // serves that particular channel, because a binding records
        // (address → principal) and nothing about workloads.
        //
        // The bounded consequence, corrected. An earlier version of this
        // comment said a stolen workload credential "can discover WHOSE
        // bookings exist", and review showed that badly understates it. Naming
        // any bound principal yields their cancellable bookings, full
        // projections, council references, purposes, dates, times, headcounts,
        // venue and status data, and audit histories. Everything a read
        // returns, which is everything.
        //
        // What it still cannot do is CHANGE one: that needs a grant, a grant
        // needs a challenge answered against a live binding, and this
        // credential's own actor is checked against the grant it presents.
        //
        // M7C's read grant, issued at binding time, is what closes this — by
        // making reading revocable in its own right rather than implied by a
        // binding existing at all.
        let store = Arc::clone(&self.store);
        let wanted = principal.clone();
        let _ = actor;
        Self::block_on(async move {
            let mut sought = None;
            // Bindings are keyed by address, so this asks the question the
            // schema can answer: is there a live binding for this principal?
            if let Ok(found) = store.live_binding_for(&wanted).await {
                sought = found;
            }
            sought.is_some()
        })
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

/// The composition root's answer to `ApprovalIssuer`.
///
/// # Why the adapter exists rather than `AuthorityService` implementing it
///
/// `townhall-http` must not depend on a concrete store or entropy source — it
/// holds a trait so that nothing in the HTTP layer knows, or can choose, which
/// issuer is behind it. This adapter is the one place that knows both, and it
/// lives in the composition root because that is what a composition root is
/// for.
///
/// It is also where the clock lives. Every method below stamps its own `now`,
/// so a request that arrives at the boundary of a deadline is judged by when it
/// arrived rather than by when the process started.
pub struct ServiceIssuer(pub Arc<AuthorityService<SqlApprovalStore, OsEntropy>>);

#[async_trait::async_trait]
impl townhall_http::approvals::ApprovalIssuer for ServiceIssuer {
    async fn begin(
        &self,
        request: &townhall_authority::ApprovalRequest,
    ) -> Result<(String, String), String> {
        let raised = self
            .0
            .begin(request, now_ms())
            .await
            .map_err(|error| error.to_string())?;
        // The PREVIEW, not the code. The code is inside the preview because the
        // person has to be told it, and returning it separately would invite a
        // caller to use it without ever sending the message.
        Ok((raised.id.as_str().to_owned(), raised.preview))
    }

    async fn reply(
        &self,
        challenge: &str,
        code: &str,
        from: &townhall_authority::BindingRef,
        assurance: townhall_authority::AssuranceLevel,
        approve: bool,
    ) -> Result<Option<String>, townhall_authority::ApprovalDenied> {
        let id = bld_types::ApprovalChallengeId::new(challenge);
        let now = now_ms();
        if approve {
            let grant = self.0.submit(&id, code, from, assurance, now).await?;
            // Only the reference crosses back (spec §13.1 step 7).
            Ok(Some(grant.delegation().as_str().to_owned()))
        } else {
            self.0.reject(&id, code, from, now).await?;
            Ok(None)
        }
    }

    async fn revoke(&self, delegation: &str) -> Result<bool, String> {
        self.0
            .revoke(&DelegationId::new(delegation), now_ms())
            .await
            .map_err(|error| error.to_string())
    }
}
