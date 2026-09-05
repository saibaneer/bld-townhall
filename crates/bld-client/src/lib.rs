#![forbid(unsafe_code)]

//! The generic BLD client (M9, spec §12; ADR-029): it discovers a service from a
//! signed manifest and drives the API from it.
//!
//! # What it hard-codes, and what it discovers
//!
//! Hard-coded: the BOOTSTRAP (a base URL + the constant `/.well-known/bld`) and
//! the BLD-generic protocol — the header names, the `if-match` concurrency rule,
//! and the manifest's own URL-template placeholders (`{id}`, `{segment}`).
//! Discovered from the manifest: every resource path and every behaviour SEGMENT.
//! Discovered live from the projection: which behaviours are legal now. The
//! client never spells a kebab behaviour segment itself — it looks each up in the
//! manifest by the `PascalCase` name the projection published (the gate: "no
//! hard-coded behaviour URLs beyond bootstrap").
//!
//! # Untrusted driver
//!
//! It resolves no authority and mutates nothing on its own: it carries an OPAQUE
//! delegation reference it was HANDED (never one it computes), and the server
//! refuses a change with no delegation. The crate graph forbids it every booking
//! and authority type — a socket is its only route — and forbids `bld-types` too,
//! so it cannot reach the segment table it is meant to discover.

use bld_manifest::{ResourceLink, SignedManifest, VerifyingKey};
use serde::Deserialize;

/// The major version of the manifest schema this client understands. It refuses a
/// manifest whose major differs, rather than drive one it cannot read (spec §12's
/// `bld_version` is exactly for this).
const SUPPORTED_MAJOR: &str = "0";

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("could not read the catalogue: {0}")]
    Catalogue(String),
    #[error("discovery transport failed: {0}")]
    Transport(String),
    #[error("the manifest did not parse: {0}")]
    BadManifest(String),
    #[error("the manifest did not verify against the pinned publisher key")]
    Unverified,
    #[error("the manifest speaks bld_version {found}, this client speaks major {SUPPORTED_MAJOR}")]
    IncompatibleVersion { found: String },
    #[error("the manifest describes no resource {0:?}")]
    UnknownResource(String),
    #[error("the manifest's {resource:?} lists no behaviour {behaviour:?}")]
    UnknownBehaviour { resource: String, behaviour: String },
    #[error("the server answered {status}: {detail}")]
    Refused { status: u16, detail: String },
    #[error("the response did not parse: {0}")]
    BadResponse(String),
}

// ------------------------------------------------------------------ catalogue

/// One service the client can discover: where it is, and the key to trust it by.
#[derive(Clone, Debug, Deserialize)]
pub struct CatalogueEntry {
    pub base_url: String,
    /// The publisher's ed25519 verifying key, 64 hex chars — the out-of-band
    /// trust anchor this entry pins the manifest to.
    pub publisher_key: String,
}

/// A tiny local marketplace catalogue (§12): a list of services, read from a
/// file. The registry stand-in — no network lookup, no ranking, no safety claim.
#[derive(Clone, Debug, Deserialize)]
pub struct Catalogue {
    services: Vec<CatalogueEntry>,
}

impl Catalogue {
    /// Read a catalogue from a JSON file: `{ "services": [ { base_url, publisher_key } ] }`.
    ///
    /// # Errors
    /// The file is unreadable or not the catalogue shape.
    pub fn from_file(path: impl AsRef<std::path::Path>) -> Result<Self, ClientError> {
        let bytes = std::fs::read(path).map_err(|e| ClientError::Catalogue(e.to_string()))?;
        serde_json::from_slice(&bytes).map_err(|e| ClientError::Catalogue(e.to_string()))
    }

    #[must_use]
    pub fn services(&self) -> &[CatalogueEntry] {
        &self.services
    }
}

// ------------------------------------------------------------------ discovery

/// A service the client has discovered and verified — the manifest plus where it
/// came from. Everything the client drives is read from here.
#[derive(Clone, Debug)]
pub struct Discovered {
    base_url: String,
    manifest: SignedManifest,
}

impl Discovered {
    /// Wrap already-obtained manifest bytes after verifying them — the same
    /// integrity + authenticity + version checks [`discover`] runs, minus the
    /// fetch. This is what [`discover`] calls once it has the bytes; it is also
    /// the seam for a caller that already holds signed bytes (a catalogue cache),
    /// and for the gate witness that drives off a manifest re-signed by the real
    /// publisher — proving the client obeys the manifest, not a hard-coded table.
    ///
    /// # Errors
    /// The manifest does not verify against `publisher_key`, or its `bld_version`
    /// major differs from this client's.
    pub fn verified(
        base_url: &str,
        manifest: SignedManifest,
        publisher_key: &VerifyingKey,
    ) -> Result<Self, ClientError> {
        manifest
            .verify(publisher_key)
            .map_err(|_| ClientError::Unverified)?;

        // Compatibility BEFORE driving: refuse a manifest whose major we do not speak.
        let major = manifest
            .manifest
            .bld_version
            .split('.')
            .next()
            .unwrap_or_default();
        if major != SUPPORTED_MAJOR {
            return Err(ClientError::IncompatibleVersion {
                found: manifest.manifest.bld_version.clone(),
            });
        }

        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            manifest,
        })
    }

    #[must_use]
    pub fn service(&self) -> &str {
        &self.manifest.manifest.service
    }

    fn resource(&self, resource: &str) -> Result<&ResourceLink, ClientError> {
        self.manifest
            .manifest
            .resource_links
            .get(resource)
            .ok_or_else(|| ClientError::UnknownResource(resource.to_owned()))
    }

    /// The wire segment for a behaviour, looked up in the manifest by the name the
    /// projection published — the ONE place the client learns a kebab segment.
    fn segment(&self, resource: &str, behaviour: &str) -> Result<String, ClientError> {
        self.resource(resource)?
            .behaviours
            .get(behaviour)
            .map(|link| link.segment.clone())
            .ok_or_else(|| ClientError::UnknownBehaviour {
                resource: resource.to_owned(),
                behaviour: behaviour.to_owned(),
            })
    }
}

/// Discover a service: GET its `/.well-known/bld`, verify the signed manifest
/// against the pinned publisher key, and check the version — all before it is
/// trusted to drive from.
///
/// # Errors
/// Transport failure, an unparseable or unverified manifest, or an incompatible
/// `bld_version`.
pub async fn discover(
    http: &reqwest::Client,
    base_url: &str,
    publisher_key: &VerifyingKey,
) -> Result<Discovered, ClientError> {
    // The ONE hard-coded path — the bootstrap the gate permits.
    let url = format!("{}/.well-known/bld", base_url.trim_end_matches('/'));
    let response = http
        .get(&url)
        .send()
        .await
        .map_err(|e| ClientError::Transport(e.to_string()))?;
    if !response.status().is_success() {
        return Err(ClientError::Transport(format!(
            "discovery returned {}",
            response.status()
        )));
    }
    let manifest: SignedManifest = response
        .json()
        .await
        .map_err(|e| ClientError::BadManifest(e.to_string()))?;

    Discovered::verified(base_url, manifest, publisher_key)
}

/// Discover the service a catalogue entry names, pinning its publisher key.
///
/// # Errors
/// The entry's key is malformed, or discovery fails.
pub async fn discover_entry(
    http: &reqwest::Client,
    entry: &CatalogueEntry,
) -> Result<Discovered, ClientError> {
    let key = bld_manifest::verifying_key_from_hex(&entry.publisher_key)
        .ok_or_else(|| ClientError::Catalogue("publisher_key is not 64 hex chars".to_owned()))?;
    discover(http, &entry.base_url, &key).await
}

// ------------------------------------------------------------------ client

/// A booking-intent projection, as the client reads it back. Re-declared
/// independently of the server's DTO (ADR-023) — agreeing over a socket is the
/// contract test.
#[derive(Clone, Debug, Deserialize)]
struct ProjectionBody {
    state: String,
    #[serde(default)]
    available_behaviours: Vec<String>,
}

/// What the client learned about a resource after a fetch or a drive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fetched {
    pub state: String,
    pub available_behaviours: Vec<String>,
    /// The resource version, read from the `ETag` — what the next change sends as
    /// `If-Match`.
    pub version: Option<u64>,
}

/// The generic driver, bound to one discovered service and one caller identity.
pub struct BldClient {
    discovered: Discovered,
    http: reqwest::Client,
    principal: String,
    bearer: String,
    /// The delegation reference — an OPAQUE token the client was HANDED, never one
    /// it computes. Sent as `x-bld-delegation`; the server resolves it and refuses
    /// a change without it. `None` means "no grant", and a change will be refused.
    delegation: Option<String>,
}

impl BldClient {
    #[must_use]
    pub fn new(
        discovered: Discovered,
        http: reqwest::Client,
        principal: impl Into<String>,
        bearer: impl Into<String>,
        delegation: Option<String>,
    ) -> Self {
        Self {
            discovered,
            http,
            principal: principal.into(),
            bearer: bearer.into(),
            delegation,
        }
    }

    #[must_use]
    pub fn discovered(&self) -> &Discovered {
        &self.discovered
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.discovered.base_url)
    }

    /// Apply the BLD-generic headers — the one place, so no call omits one.
    fn authorized(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let builder = builder
            .header("authorization", format!("Bearer {}", self.bearer))
            .header("x-bld-principal", &self.principal);
        match &self.delegation {
            Some(reference) => builder.header("x-bld-delegation", reference),
            None => builder,
        }
    }

    /// Create a resource item. The collection PATH comes from the manifest; the
    /// body is the caller's (the resource's representation, not a behaviour URL).
    ///
    /// # Errors
    /// The resource is unknown, the transport failed, or the server refused.
    pub async fn create(
        &self,
        resource: &str,
        body: serde_json::Value,
    ) -> Result<Fetched, ClientError> {
        let path = self.discovered.resource(resource)?.collection.clone();
        let request = self.authorized(self.http.post(self.url(&path))).json(&body);
        self.send(request).await
    }

    /// Read a resource item. The item PATH template comes from the manifest.
    ///
    /// # Errors
    /// The resource is unknown, the transport failed, or the server refused.
    pub async fn read(&self, resource: &str, id: &str) -> Result<Fetched, ClientError> {
        let path = self.discovered.resource(resource)?.item.replace("{id}", id);
        let request = self.authorized(self.http.get(self.url(&path)));
        self.send(request).await
    }

    /// Drive a behaviour by the NAME the projection published. The URL is built
    /// from the manifest's template + the segment looked up for that name — the
    /// client spells no segment itself. `if_match` is the version the read
    /// observed (optimistic concurrency).
    ///
    /// # Errors
    /// The behaviour is not in the manifest, the transport failed, or the server
    /// refused (a 404 here means the segment named no route — the manifest and the
    /// server disagree).
    pub async fn drive(
        &self,
        resource: &str,
        id: &str,
        behaviour: &str,
        body: serde_json::Value,
        if_match: Option<u64>,
    ) -> Result<Fetched, ClientError> {
        let link = self.discovered.resource(resource)?;
        let segment = self.discovered.segment(resource, behaviour)?;
        let path = link
            .behaviour_template
            .replace("{id}", id)
            .replace("{segment}", &segment);
        let mut request = self.authorized(self.http.post(self.url(&path))).json(&body);
        if let Some(version) = if_match {
            request = request.header("if-match", format!("\"{version}\""));
        }
        self.send(request).await
    }

    /// GET a read-only path under the discovered service and return its JSON body
    /// verbatim — the permitted read-only surface a proposer searches for venue
    /// candidates over (spec §18.3). It carries the caller's identity like any
    /// call, so the server scopes it; it drives nothing and sends no version.
    ///
    /// # Errors
    /// The transport failed, or the server refused.
    pub async fn browse(&self, path: &str) -> Result<serde_json::Value, ClientError> {
        let response = self
            .authorized(self.http.get(self.url(path)))
            .send()
            .await
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let detail = response.text().await.unwrap_or_default();
            return Err(ClientError::Refused { status, detail });
        }
        response
            .json()
            .await
            .map_err(|e| ClientError::BadResponse(e.to_string()))
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<Fetched, ClientError> {
        let response = request
            .send()
            .await
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        let status = response.status().as_u16();
        let version = response
            .headers()
            .get("etag")
            .and_then(|value| value.to_str().ok())
            .and_then(|raw| raw.trim_matches('"').parse::<u64>().ok());
        if !(200..300).contains(&status) {
            let detail = response.text().await.unwrap_or_default();
            return Err(ClientError::Refused { status, detail });
        }
        let body: ProjectionBody = response
            .json()
            .await
            .map_err(|e| ClientError::BadResponse(e.to_string()))?;
        Ok(Fetched {
            state: body.state,
            available_behaviours: body.available_behaviours,
            version,
        })
    }
}
