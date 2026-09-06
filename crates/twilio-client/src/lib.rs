#![forbid(unsafe_code)]

//! Twilio Programmable Messaging over HTTP — the M12 real-SMS adapter's REST leg.
//!
//! A trusted transport adapter, in the spirit of `stripe-client`: it carries a
//! canonical outbound reply to Twilio and returns the provider's raw observation
//! (the message SID and queue status). It asserts nothing about identity — that
//! authority stays in the channel/domain layers — and it holds the one secret
//! (an Auth Token or API Key Secret) behind a type whose `Debug` never reveals it.
//!
//! Two operations, both Twilio's protocol, not ours:
//! - [`TwilioClient::send_sms`] — a basic-auth `POST` to `Messages.json`.
//! - [`verify_signature`] — the `X-Twilio-Signature` check Twilio documents:
//!   `base64(HMAC-SHA1(auth_token, url + params-sorted-by-key))`, compared in
//!   constant time. This is how an inbound webhook proves it came from Twilio and
//!   was not tampered with, using only the account's own Auth Token.

use std::collections::BTreeMap;
use std::fmt;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use hmac::{Hmac, Mac, digest::KeyInit};
use serde::Deserialize;
use sha1::Sha1;
use thiserror::Error;

/// A Twilio API secret whose `Debug` never reveals it — either an Account **Auth
/// Token** or an **API Key Secret**, depending on which credential is configured.
#[derive(Clone)]
pub struct TwilioSecret(String);

impl TwilioSecret {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The raw secret, for the one place it must cross a boundary: HTTP basic auth
    /// and signature verification. Named to make its use conspicuous at call
    /// sites, the way `stripe-client` guards its secret key.
    #[must_use]
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for TwilioSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TwilioSecret(****)")
    }
}

/// Everything the adapter needs told to it, never assumed.
///
/// Twilio supports two credential shapes, and they differ in a way that matters:
/// the resource URL always carries the **Account SID** (`AC…`), while the
/// basic-auth username is either that same Account SID (Auth-Token auth) OR a
/// separate **API Key SID** (`SK…`, the recommended, revocable credential). So
/// the path SID and the auth username are two distinct fields — conflating them
/// is exactly what makes an API Key return `401` "no requested permission".
#[derive(Clone, Debug)]
pub struct TwilioConfig {
    /// The Account SID (`AC…`) — the resource owner, in the API URL path.
    pub account_sid: String,
    /// The basic-auth username: the API Key SID (`SK…`) or, in Auth-Token auth,
    /// the Account SID again.
    pub auth_sid: String,
    /// The basic-auth password: the API Key Secret or the Account Auth Token.
    pub auth_secret: TwilioSecret,
    /// The SMS-capable number we send FROM, in E.164 (e.g. `+447723317807`).
    pub from_number: String,
}

impl TwilioConfig {
    /// Read the config from a getter (usually `|n| std::env::var(n).ok()`), so a
    /// test can supply values without touching the process environment.
    ///
    /// Env names match the running `.env`: `TWILIO_SID` (the basic-auth username —
    /// an `SK…` API Key SID or an `AC…` Account SID), `TWILIO_CLIENT_SECRET` (its
    /// secret) and `TWILIO_FROM_NUMBER`. The URL-path Account SID comes from
    /// `TWILIO_ACCOUNT_SID` when set, else falls back to `TWILIO_SID` (the
    /// Auth-Token case, where the two coincide). An empty value is treated as
    /// unset — a blank secret must never silently "work".
    ///
    /// # Errors
    /// [`TwilioError::Config`] naming a missing variable, or
    /// [`TwilioError::AccountSid`] when the path SID is not an `AC…` Account SID
    /// (the tell-tale of an API Key SID left in `TWILIO_SID` with no
    /// `TWILIO_ACCOUNT_SID` to accompany it).
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Result<Self, TwilioError> {
        let present = |name: &str| get(name).filter(|value| !value.trim().is_empty());
        let read = |name: &'static str| present(name).ok_or(TwilioError::Config(name));

        let auth_sid = read("TWILIO_SID")?;
        let account_sid = present("TWILIO_ACCOUNT_SID").unwrap_or_else(|| auth_sid.clone());
        if !account_sid.starts_with("AC") {
            return Err(TwilioError::AccountSid);
        }
        Ok(Self {
            account_sid,
            auth_sid,
            auth_secret: TwilioSecret::new(read("TWILIO_CLIENT_SECRET")?),
            from_number: read("TWILIO_FROM_NUMBER")?,
        })
    }
}

/// The provider's raw observation of an accepted send — never a domain fact.
#[derive(Clone, Debug, Deserialize)]
pub struct SentMessage {
    /// Twilio's message SID (`SM…`) — the idempotency handle for dedupe.
    pub sid: String,
    /// The queue status Twilio reported (`queued`, `sent`, …).
    pub status: String,
}

/// Everything the send can answer that is not a `SentMessage`.
#[derive(Debug, Error)]
pub enum TwilioError {
    /// A required config value is unset or empty.
    #[error("{0} is unset")]
    Config(&'static str),
    /// The URL-path Account SID is not an `AC…` value — usually an API Key SID
    /// (`SK…`) left in `TWILIO_SID` without a `TWILIO_ACCOUNT_SID` beside it.
    #[error(
        "TWILIO_ACCOUNT_SID must be the AC… Account SID (TWILIO_SID looks like an API Key SID)"
    )]
    AccountSid,
    /// The HTTP call itself failed (DNS, TLS, connection).
    #[error("twilio transport error: {0}")]
    Transport(String),
    /// Twilio answered non-2xx. Carries the status and its (non-secret) body.
    #[error("twilio API error {status}: {body}")]
    Api { status: u16, body: String },
    /// A 2xx body that did not parse as a message resource.
    #[error("unrecognized twilio response: {0}")]
    BadResponse(String),
}

/// Talks to one Twilio-compatible Messaging API.
pub struct TwilioClient {
    http: reqwest::Client,
    base_url: String,
    config: TwilioConfig,
}

impl TwilioClient {
    #[must_use]
    pub fn new(http: reqwest::Client, config: TwilioConfig) -> Self {
        Self {
            http,
            base_url: "https://api.twilio.com".to_owned(),
            config,
        }
    }

    /// Point the client at a different base URL — for a hermetic mock in tests.
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Send one SMS: a basic-auth `POST` to `Messages.json`, form-encoded exactly
    /// as Twilio's REST API expects. Returns the provider's raw observation.
    ///
    /// A non-2xx is an [`TwilioError::Api`], not a panic — Twilio's error body
    /// (e.g. an unverified trial recipient) is information the caller must see.
    ///
    /// # Errors
    /// [`TwilioError::Transport`], [`TwilioError::Api`], [`TwilioError::BadResponse`].
    pub async fn send_sms(&self, to: &str, body: &str) -> Result<SentMessage, TwilioError> {
        let url = format!(
            "{}/2010-04-01/Accounts/{}/Messages.json",
            self.base_url, self.config.account_sid
        );
        let response = self
            .http
            .post(&url)
            .basic_auth(
                &self.config.auth_sid,
                Some(self.config.auth_secret.expose_secret()),
            )
            .form(&[
                ("To", to),
                ("From", self.config.from_number.as_str()),
                ("Body", body),
            ])
            .send()
            .await
            .map_err(|error| TwilioError::Transport(error.to_string()))?;

        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(TwilioError::Api { status, body: text });
        }
        serde_json::from_str(&text).map_err(|error| TwilioError::BadResponse(error.to_string()))
    }
}

/// Verify an `X-Twilio-Signature`.
///
/// Twilio's documented scheme: take the full request URL, append every POST
/// parameter as `key` immediately followed by `value` in **key-sorted** order,
/// HMAC-SHA1 it with the Auth Token, base64 the digest, and compare. A
/// [`BTreeMap`] gives the sort for free. The comparison is constant-time
/// (`Mac::verify_slice`), so a caller cannot learn the expected MAC byte by byte.
///
/// Returns `false` — never an error — for a bad signature, a non-base64 header,
/// a tampered parameter, the wrong URL, or the wrong token. `false` is the only
/// safe answer to "should I trust this webhook?".
#[must_use]
pub fn verify_signature(
    auth_token: &str,
    url: &str,
    params: &BTreeMap<String, String>,
    provided: &str,
) -> bool {
    let mut signed = String::from(url);
    for (key, value) in params {
        signed.push_str(key);
        signed.push_str(value);
    }
    let Ok(provided_bytes) = BASE64.decode(provided) else {
        return false;
    };
    // HMAC accepts a key of any length, so `new_from_slice` cannot actually fail
    // here; treating a (impossible) key error as "unverified" keeps this function
    // panic-free and total — `false` is the only safe answer either way.
    let Ok(mut mac) = Hmac::<Sha1>::new_from_slice(auth_token.as_bytes()) else {
        return false;
    };
    mac.update(signed.as_bytes());
    mac.verify_slice(&provided_bytes).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical_params() -> BTreeMap<String, String> {
        // Twilio's own documented example set.
        [
            ("CallSid", "CA1234567890ABCDE"),
            ("Caller", "+14158675310"),
            ("Digits", "1234"),
            ("From", "+14158675310"),
            ("To", "+18005551212"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
    }

    const CANONICAL_URL: &str = "https://mycompany.com/myapp.php?foo=1&bar=2";
    // Computed INDEPENDENTLY with `openssl dgst -sha1 -hmac 12345 | openssl base64`
    // over the canonical (url + sorted params) string — the oracle a wrong
    // implementation (wrong sort, concat, algorithm, or encoding) cannot match.
    const CANONICAL_SIG: &str = "GvWf1cFY/Q7PnoempGyD5oXAezc=";

    #[test]
    fn verify_matches_the_independent_reference_signature() {
        assert!(verify_signature(
            "12345",
            CANONICAL_URL,
            &canonical_params(),
            CANONICAL_SIG
        ));
    }

    #[test]
    fn verify_rejects_every_tampering() {
        let params = canonical_params();
        // A flipped signature byte.
        assert!(!verify_signature(
            "12345",
            CANONICAL_URL,
            &params,
            "GvWf1cFY/Q7PnoempGyD5oXAezd="
        ));
        // The wrong token.
        assert!(!verify_signature(
            "54321",
            CANONICAL_URL,
            &params,
            CANONICAL_SIG
        ));
        // A tampered parameter.
        let mut tampered = params.clone();
        tampered.insert("Digits".to_owned(), "9999".to_owned());
        assert!(!verify_signature(
            "12345",
            CANONICAL_URL,
            &tampered,
            CANONICAL_SIG
        ));
        // The wrong URL.
        assert!(!verify_signature(
            "12345",
            "https://evil.example/webhook",
            &params,
            CANONICAL_SIG
        ));
        // A non-base64 header is refused, not panicked.
        assert!(!verify_signature(
            "12345",
            CANONICAL_URL,
            &params,
            "not base64!!"
        ));
    }

    #[test]
    fn verify_is_order_independent_because_it_sorts() {
        // The same params inserted in a different order must still verify: the
        // BTreeMap sort, not insertion order, is what the signature is over.
        let mut reversed = BTreeMap::new();
        reversed.insert("To".to_owned(), "+18005551212".to_owned());
        reversed.insert("From".to_owned(), "+14158675310".to_owned());
        reversed.insert("Digits".to_owned(), "1234".to_owned());
        reversed.insert("Caller".to_owned(), "+14158675310".to_owned());
        reversed.insert("CallSid".to_owned(), "CA1234567890ABCDE".to_owned());
        assert!(verify_signature(
            "12345",
            CANONICAL_URL,
            &reversed,
            CANONICAL_SIG
        ));
    }

    fn from(env: &[(&str, &str)]) -> Result<TwilioConfig, TwilioError> {
        let map: BTreeMap<&str, &str> = env.iter().copied().collect();
        TwilioConfig::from_env(|name| map.get(name).map(|v| (*v).to_owned()))
    }

    #[test]
    fn api_key_auth_keeps_the_account_sid_and_the_key_sid_apart() {
        // The recommended shape: an SK… key SID authenticates, but the AC… Account
        // SID owns the resource path. Conflating them is the 401 we hit for real.
        let config = from(&[
            ("TWILIO_SID", "SK999"),
            ("TWILIO_CLIENT_SECRET", "key-secret"),
            ("TWILIO_ACCOUNT_SID", "AC123"),
            ("TWILIO_FROM_NUMBER", "+447723317807"),
        ])
        .expect("all four present");
        assert_eq!(config.account_sid, "AC123", "the AC… owns the URL path");
        assert_eq!(
            config.auth_sid, "SK999",
            "the SK… is the basic-auth username"
        );
        assert_eq!(config.auth_secret.expose_secret(), "key-secret");
        assert_eq!(config.from_number, "+447723317807");
    }

    #[test]
    fn auth_token_auth_lets_the_account_sid_stand_in_for_both() {
        // The simpler shape: an AC… Account SID + Auth Token, no separate key. The
        // path SID falls back to TWILIO_SID when TWILIO_ACCOUNT_SID is absent.
        let config = from(&[
            ("TWILIO_SID", "AC123"),
            ("TWILIO_CLIENT_SECRET", "auth-token"),
            ("TWILIO_FROM_NUMBER", "+441"),
        ])
        .expect("present");
        assert_eq!(config.account_sid, "AC123");
        assert_eq!(config.auth_sid, "AC123");
    }

    #[test]
    fn config_refuses_an_api_key_sid_with_no_account_sid_beside_it() {
        // Exactly the misconfiguration behind the live 401: an SK… in TWILIO_SID
        // and no TWILIO_ACCOUNT_SID, so the path SID would not be an Account SID.
        assert!(matches!(
            from(&[
                ("TWILIO_SID", "SK999"),
                ("TWILIO_CLIENT_SECRET", "key-secret"),
                ("TWILIO_FROM_NUMBER", "+441"),
            ]),
            Err(TwilioError::AccountSid)
        ));
    }

    #[test]
    fn config_refuses_a_missing_or_blank_value_naming_it() {
        // Missing secret: the error NAMES the variable.
        match from(&[("TWILIO_SID", "AC1"), ("TWILIO_FROM_NUMBER", "+441")]) {
            Err(TwilioError::Config("TWILIO_CLIENT_SECRET")) => {}
            other => panic!("must name the missing secret: {other:?}"),
        }
        // A blank secret is treated as unset, not accepted.
        assert!(matches!(
            from(&[
                ("TWILIO_SID", "AC1"),
                ("TWILIO_CLIENT_SECRET", "   "),
                ("TWILIO_FROM_NUMBER", "+441"),
            ]),
            Err(TwilioError::Config("TWILIO_CLIENT_SECRET"))
        ));
    }

    #[tokio::test]
    async fn send_sms_encodes_the_request_twilio_expects() {
        use std::sync::{Arc, Mutex};

        use axum::extract::{Path, State};
        use axum::http::HeaderMap;
        use axum::routing::post;
        use axum::{Json, Router};

        // The mock captures what our client actually put on the wire.
        #[derive(Default)]
        struct Captured {
            sid_in_path: String,
            authorization: String,
            body: String,
        }
        let captured = Arc::new(Mutex::new(Captured::default()));

        let app = Router::new()
            .route(
                "/2010-04-01/Accounts/{sid}/Messages.json",
                post(
                    |Path(sid): Path<String>,
                     State(cap): State<Arc<Mutex<Captured>>>,
                     headers: HeaderMap,
                     body: String| async move {
                        let mut c = cap.lock().expect("lock");
                        c.sid_in_path = sid;
                        c.authorization = headers
                            .get("authorization")
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_owned();
                        c.body = body;
                        Json(serde_json::json!({"sid": "SM_MOCK_1", "status": "queued"}))
                    },
                ),
            )
            .with_state(Arc::clone(&captured));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });

        // API-key shape: the AC… owns the path, the SK… authenticates — the two
        // must NOT be conflated (the real 401 was exactly that conflation).
        let config = TwilioConfig {
            account_sid: "AC777".to_owned(),
            auth_sid: "SK999".to_owned(),
            auth_secret: TwilioSecret::new("key-secret"),
            from_number: "+447723317807".to_owned(),
        };
        let client = TwilioClient::new(reqwest::Client::new(), config)
            .with_base_url(format!("http://{addr}"));

        let sent = client
            .send_sms("+447760805996", "hello from the boundary")
            .await
            .expect("the mock accepts the send");
        assert_eq!(sent.sid, "SM_MOCK_1");
        assert_eq!(sent.status, "queued");

        let c = captured.lock().expect("lock");
        assert_eq!(
            c.sid_in_path, "AC777",
            "the ACCOUNT SID is in the URL path, not the key SID"
        );
        assert_eq!(
            c.authorization,
            format!("Basic {}", BASE64.encode("SK999:key-secret")),
            "basic auth is the KEY SID and its secret, not the account SID"
        );
        // The three form fields, url-encoded exactly as Twilio's API expects.
        assert!(c.body.contains("To=%2B447760805996"), "To: {}", c.body);
        assert!(c.body.contains("From=%2B447723317807"), "From: {}", c.body);
        assert!(
            c.body.contains("Body=hello+from+the+boundary"),
            "Body: {}",
            c.body
        );
    }

    #[test]
    fn the_secret_never_prints_itself() {
        let secret = TwilioSecret::new("super-secret-value");
        assert_eq!(format!("{secret:?}"), "TwilioSecret(****)");
        // And the whole config's Debug carries no secret.
        let config = TwilioConfig {
            account_sid: "AC1".to_owned(),
            auth_sid: "SK1".to_owned(),
            auth_secret: secret,
            from_number: "+441".to_owned(),
        };
        assert!(!format!("{config:?}").contains("super-secret-value"));
    }
}
