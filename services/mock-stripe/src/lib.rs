#![forbid(unsafe_code)]

//! A hermetic Stripe Checkout HTTP double with provider-side idempotency.

#[cfg(feature = "test-faults")]
pub mod faults;

use axum::{
    Form, Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse as _, Response},
    routing::{get, post},
};
use serde::Serialize;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone)]
pub struct MockStripe {
    state: AppState,
}

impl Default for MockStripe {
    fn default() -> Self {
        Self::new()
    }
}

impl MockStripe {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: AppState {
                sessions: Arc::new(Mutex::new(Sessions::default())),
                #[cfg(feature = "test-faults")]
                faults: Arc::new(faults::FaultBank::default()),
            },
        }
    }

    pub fn router(&self) -> Router {
        let router = Router::new()
            .route("/v1/checkout/sessions", post(create_session))
            .route("/v1/checkout/sessions/{session_id}", get(get_session));

        #[cfg(feature = "test-faults")]
        let router = router
            .route("/test/faults", post(arm_fault))
            .route("/test/faults/{fault_id}", get(fault_status));

        router.with_state(self.state.clone())
    }
}

#[derive(Clone)]
struct AppState {
    sessions: Arc<Mutex<Sessions>>,
    #[cfg(feature = "test-faults")]
    faults: Arc<faults::FaultBank>,
}

#[derive(Default)]
struct Sessions {
    next_id: u64,
    by_id: HashMap<String, Session>,
    by_key: HashMap<String, String>,
    by_payment: HashMap<String, String>,
}

#[derive(Clone, Serialize)]
struct Session {
    id: String,
    url: String,
    expires_at: i64,
    metadata: Metadata,
    status: String,
    payment_status: String,
    payment_intent: PaymentIntent,
}

#[derive(Clone, Serialize)]
struct Metadata {
    payment_intent_id: String,
}

#[derive(Clone, Serialize)]
struct PaymentIntent {
    id: String,
    status: String,
}

struct CreateSession {
    amount: u64,
    currency: String,
    payment_intent_id: String,
    success_url: String,
    cancel_url: String,
}

impl CreateSession {
    fn parse(form: &HashMap<String, String>) -> Result<Self, &'static str> {
        let required = |key: &str| form.get(key).cloned().ok_or("missing required field");
        Ok(Self {
            amount: required("line_items[0][price_data][unit_amount]")?
                .parse()
                .map_err(|_| "amount must be minor units")?,
            currency: required("line_items[0][price_data][currency]")?,
            payment_intent_id: required("metadata[payment_intent_id]")?,
            success_url: required("success_url")?,
            cancel_url: required("cancel_url")?,
        })
    }

    fn validate(&self) -> Result<(), &'static str> {
        if self.amount == 0 {
            return Err("amount must be positive");
        }
        if self.currency != "gbp" {
            return Err("currency must be gbp");
        }
        if self.success_url.is_empty() || self.cancel_url.is_empty() {
            return Err("redirect URLs must be present");
        }
        Ok(())
    }
}

async fn create_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let Ok(request) = CreateSession::parse(&form) else {
        return stripe_error(StatusCode::BAD_REQUEST, "invalid checkout form");
    };
    if let Err(message) = request.validate() {
        return stripe_error(StatusCode::BAD_REQUEST, message);
    }
    let idempotency_key = headers
        .get("Idempotency-Key")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let scope = idempotency_key
        .as_deref()
        .unwrap_or(&request.payment_intent_id)
        .to_owned();

    // Classification and creation share one lock. Existing is classified first,
    // and the new session is committed to every index before any response fault.
    let session = {
        let mut sessions = state
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let existing = idempotency_key
            .as_ref()
            .and_then(|key| sessions.by_key.get(key))
            .or_else(|| sessions.by_payment.get(&request.payment_intent_id))
            .cloned();
        if let Some(session_id) = existing {
            if let Some(key) = idempotency_key {
                sessions
                    .by_key
                    .entry(key)
                    .or_insert_with(|| session_id.clone());
            }
            sessions
                .by_id
                .get(&session_id)
                .cloned()
                .expect("session indexes remain coherent")
        } else {
            sessions.next_id += 1;
            let session_id = format!("cs_test_{:08}", sessions.next_id);
            let session = Session {
                id: session_id.clone(),
                url: format!("https://checkout.stripe.test/{session_id}"),
                expires_at: now_seconds().saturating_add(86_400),
                metadata: Metadata {
                    payment_intent_id: request.payment_intent_id.clone(),
                },
                status: "open".to_owned(),
                payment_status: "unpaid".to_owned(),
                payment_intent: PaymentIntent {
                    id: format!("pi_mock_{}", request.payment_intent_id),
                    status: "requires_payment_method".to_owned(),
                },
            };
            if let Some(key) = idempotency_key {
                sessions.by_key.insert(key, session_id.clone());
            }
            sessions
                .by_payment
                .insert(request.payment_intent_id, session_id.clone());
            sessions.by_id.insert(session_id, session.clone());
            session
        }
    };

    #[cfg(feature = "test-faults")]
    if let Some(fault) = state.faults.consume(&scope, faults::Route::Create) {
        return misbehave(fault, session).await;
    }

    Json(session).into_response()
}

async fn get_session(State(state): State<AppState>, Path(session_id): Path<String>) -> Response {
    let session = state
        .sessions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .by_id
        .get(&session_id)
        .cloned();
    let Some(session) = session else {
        return stripe_error(StatusCode::NOT_FOUND, "unknown checkout session");
    };

    #[cfg(feature = "test-faults")]
    if let Some(fault) = state.faults.consume(&session_id, faults::Route::Retrieve) {
        return misbehave(fault, session).await;
    }

    Json(session).into_response()
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or(0)
}

fn stripe_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(serde_json::json!({ "error": { "message": message } })),
    )
        .into_response()
}

#[cfg(feature = "test-faults")]
async fn arm_fault(
    State(state): State<AppState>,
    Json(request): Json<faults::ArmRequest>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "fault_id": state.faults.arm(request) }))
}

#[cfg(feature = "test-faults")]
async fn fault_status(State(state): State<AppState>, Path(fault_id): Path<u64>) -> Response {
    state.faults.status(fault_id).map_or_else(
        || StatusCode::NOT_FOUND.into_response(),
        |status| Json(status).into_response(),
    )
}

#[cfg(feature = "test-faults")]
async fn misbehave(fault: faults::Fault, session: Session) -> Response {
    match fault {
        faults::Fault::DropResponse => {
            let broken = futures_util::stream::iter([Err::<axum::body::Bytes, std::io::Error>(
                std::io::Error::other("response dropped by armed fault"),
            )]);
            Response::builder()
                .status(StatusCode::OK)
                .body(axum::body::Body::from_stream(broken))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        faults::Fault::Delay { ms } => {
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            Json(session).into_response()
        }
        faults::Fault::Garbage => (StatusCode::OK, "this is not Stripe JSON {{{").into_response(),
    }
}
