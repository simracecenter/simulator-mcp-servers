// SPDX-License-Identifier: GPL-3.0-or-later

//! HTTP transport — Azure AD client_credentials token acquisition and batch POST.
//!
//! Token acquisition uses the Azure AD OAuth2 `client_credentials` endpoint
//! directly via `ureq`, implementing the same flow as `azure_identity`'s
//! `ClientSecretCredential` without pulling in an async runtime. This keeps
//! the transport fully synchronous and consistent with the binary's design
//! principle of deterministic, GC-free frame processing.
//!
//! # Wire format (POST `/api/publisher/v2/ingest`)
//!
//! ```json
//! {
//!   "subSessionId": 12345678,
//!   "sessionTime": 1234.5,
//!   "sessionTick": 9876,
//!   "events": [ /* Vec<PublisherEvent> */ ]
//! }
//! ```

use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::{log_info, log_warn, publisher_event::PublisherEvent};

// ── Constants ─────────────────────────────────────────────────────────────────

/// Maximum number of events in a single POST body.
pub const BATCH_LIMIT: usize = 20;

/// Retry delays (ms) for 5xx / network errors. Three attempts after initial.
const RETRY_DELAYS_MS: &[u64] = &[500, 1_000, 2_000];

/// Refresh the cached token this many seconds before it actually expires.
const TOKEN_REFRESH_BUFFER_S: u64 = 60;

// ── Public types ──────────────────────────────────────────────────────────────

/// Category of a [`TransportError`] — surfaced to `status.json` as
/// `lastErrorKind` via [`TransportErrorKind::label`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportErrorKind {
    /// Authentication failed (bad/expired credentials, rejected token).
    Auth,
    /// The server returned a non-2xx HTTP status.
    Http(u16),
    /// Network-level failure (connect, reset, timeout).
    Network,
    /// TLS failure (handshake, certificate, fingerprint mismatch).
    Tls,
    /// The transport itself is misconfigured.
    Config,
}

impl TransportErrorKind {
    /// Stable lowercase label for status reporting.
    pub fn label(&self) -> String {
        match self {
            TransportErrorKind::Auth => "auth".to_owned(),
            TransportErrorKind::Http(code) => format!("http_{code}"),
            TransportErrorKind::Network => "network".to_owned(),
            TransportErrorKind::Tls => "tls".to_owned(),
            TransportErrorKind::Config => "config".to_owned(),
        }
    }
}

/// Errors emitted by [`PublisherTransport`].
#[derive(Debug)]
pub struct TransportError {
    pub kind: TransportErrorKind,
    /// Human-readable detail. Never contains credentials — all messages are
    /// passed through [`PublisherTransport::redact`] before construction.
    pub message: String,
}

impl TransportError {
    pub fn new(kind: TransportErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for TransportError {}

/// Classify a `ureq` error into a [`TransportErrorKind`].
fn classify_ureq_error(e: &ureq::Error) -> TransportErrorKind {
    match e {
        ureq::Error::Status(401 | 403, _) => TransportErrorKind::Auth,
        ureq::Error::Status(code, _) => TransportErrorKind::Http(*code),
        ureq::Error::Transport(_) => {
            let msg = e.to_string().to_lowercase();
            if msg.contains("certificate") || msg.contains("tls") || msg.contains("handshake") {
                TransportErrorKind::Tls
            } else {
                TransportErrorKind::Network
            }
        }
    }
}

/// How the transport authenticates to the ingest endpoint.
enum Credential {
    /// Azure AD client-credentials flow (Race Control).
    Entra {
        client_id: String,
        client_secret: String,
        scope: String,
        token_url: String,
        cached_token: Option<CachedToken>,
    },
    /// Static bearer token (local destination).
    Static(String),
}

/// Per-batch acknowledgement parsed from the ingest response body.
///
/// The Director receiver answers `202` with
/// `{accepted, rejected, duplicate, spooled}`; when a receiver returns an empty
/// or unparseable body, `accepted` falls back to the posted batch size — a 2xx
/// acknowledgement covers the whole batch.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BatchReceipt {
    /// HTTP status of the acknowledging response (0 in dry-run).
    pub http_status: u16,
    pub accepted: usize,
    pub rejected: usize,
    pub duplicate: usize,
    /// Receiver persisted the batch for later delivery to Core instead of
    /// accepting it immediately. Still an acknowledgement.
    pub spooled: bool,
}

/// Synchronous HTTP transport that acquires tokens and batch-POSTs serialized
/// ingest bodies. Queueing, durability and pacing live in
/// [`crate::delivery::DeliveryService`] — this type performs one POST at a time
/// and owns no event buffer.
pub struct PublisherTransport {
    credential: Credential,
    ingest_url: String,
    agent: ureq::Agent,
    batch_interval_ms: u64,
    /// When `true`, batches are pretty-printed to stdout instead of POSTed.
    /// Enabled by the `--dry-run` flag on the publisher binary.
    dry_run: bool,
    /// Retry delays (ms) applied after the initial attempt. Overridable in
    /// tests via `set_retry_delays_for_test` and cleared on shutdown drain.
    retry_delays: Vec<u64>,
}

impl PublisherTransport {
    /// Create a new transport.
    ///
    /// `rc_api_url` is the Race Control base URL (no trailing slash),
    /// e.g. `"https://simracecenter.com"`.
    pub fn new(
        tenant_id: impl Into<String>,
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        scope: impl Into<String>,
        rc_api_url: impl Into<String>,
        batch_interval_ms: u64,
    ) -> Self {
        let tenant_id = tenant_id.into();
        let base_url = rc_api_url.into();
        let token_url = format!("https://login.microsoftonline.com/{tenant_id}/oauth2/v2.0/token");
        let ingest_url = format!("{base_url}/api/publisher/v2/ingest");

        Self {
            credential: Credential::Entra {
                client_id: client_id.into(),
                client_secret: client_secret.into(),
                scope: scope.into(),
                token_url,
                cached_token: None,
            },
            ingest_url,
            agent: ureq::AgentBuilder::new().build(),
            batch_interval_ms,
            dry_run: false,
            retry_delays: RETRY_DELAYS_MS.to_vec(),
        }
    }

    /// Create a transport for `destination = "local"`: posts the same
    /// `IngestRequest` envelope to `{url}/api/publisher/v2/ingest` with a
    /// static `Authorization: Bearer <token>` header. When
    /// `cert_fingerprint` is given the TLS connection is pinned to that
    /// certificate's SHA-256 fingerprint.
    pub fn new_local(
        url: &str,
        token: &str,
        cert_fingerprint: Option<&str>,
        batch_interval_ms: u64,
    ) -> Result<Self, TransportError> {
        let base_url = url.trim_end_matches('/');
        let ingest_url = format!("{base_url}/api/publisher/v2/ingest");

        let agent = match cert_fingerprint {
            Some(fp) => crate::tls_pin::pinned_agent(fp).map_err(|e| {
                TransportError::new(
                    TransportErrorKind::Config,
                    format!("invalid cert_fingerprint: {e}"),
                )
            })?,
            None => ureq::AgentBuilder::new().build(),
        };

        Ok(Self {
            credential: Credential::Static(token.to_owned()),
            ingest_url,
            agent,
            batch_interval_ms,
            dry_run: false,
            retry_delays: RETRY_DELAYS_MS.to_vec(),
        })
    }

    /// Enable dry-run mode: batches are printed to stdout, no HTTP calls are made.
    pub fn set_dry_run(&mut self, dry_run: bool) {
        self.dry_run = dry_run;
    }

    /// Batch pacing interval configured for this transport.
    pub fn batch_interval(&self) -> Duration {
        Duration::from_millis(self.batch_interval_ms)
    }

    /// Whether dry-run mode is active (batches are printed, never POSTed).
    pub fn is_dry_run(&self) -> bool {
        self.dry_run
    }

    /// Warm up authentication by ensuring a valid bearer token is available.
    ///
    /// This is safe to call at startup: the normal ingest path still refreshes
    /// tokens as needed, so a token that expires before first event publish is
    /// automatically replaced during `post_batch`. A no-op for static-token
    /// (local) credentials and in dry-run mode — neither touches the network.
    pub fn warmup_auth(&mut self) -> Result<(), TransportError> {
        if self.dry_run || matches!(self.credential, Credential::Static(_)) {
            return Ok(());
        }
        let _ = self.get_or_refresh_token(false)?;
        Ok(())
    }

    /// Build the ingest body for `events` and POST it with the configured
    /// retry schedule. The caller is responsible for persisting the body
    /// before this call when durability is required — this method does not
    /// buffer anything.
    pub fn post_events(
        &mut self,
        events: &[PublisherEvent],
        session_time: f64,
        session_tick: i64,
        sub_session_id: i64,
    ) -> Result<BatchReceipt, TransportError> {
        let body = serde_json::to_value(IngestRequest {
            sub_session_id,
            session_time,
            session_tick,
            events,
        })
        .expect("IngestRequest is always serialisable");
        self.post_body(&body)
    }

    /// POST a pre-serialized ingest body (typically read back from the durable
    /// outbox) with the configured retry schedule.
    pub fn post_body(
        &mut self,
        body_value: &serde_json::Value,
    ) -> Result<BatchReceipt, TransportError> {
        let event_count = body_value
            .get("events")
            .and_then(|e| e.as_array())
            .map(|a| a.len())
            .unwrap_or(0);

        if self.dry_run {
            let pretty = serde_json::to_string_pretty(body_value)
                .expect("IngestRequest is always serialisable");
            println!(
                "[dry-run] POST {} — {} event(s):\n{}",
                self.ingest_url, event_count, pretty
            );
            // A printed batch is not an acknowledgement — report zero accepted
            // so dry-run never inflates delivered counters.
            return Ok(BatchReceipt::default());
        }

        // Retry loop: initial attempt + up to `retry_delays.len()` retries on
        // 5xx/network error.
        let total_attempts = self.retry_delays.len() + 1;
        let delays: Vec<u64> = std::iter::once(0u64)
            .chain(self.retry_delays.iter().copied())
            .collect();
        let mut last_error = String::new();
        let mut last_kind = TransportErrorKind::Network;

        for (attempt, delay_ms) in delays.into_iter().enumerate() {
            if delay_ms > 0 {
                std::thread::sleep(Duration::from_millis(delay_ms));
            }

            let token = self.get_or_refresh_token(false)?;
            let result = self
                .agent
                .post(&self.ingest_url)
                .set("Authorization", &format!("Bearer {token}"))
                .set("Content-Type", "application/json")
                .send_json(body_value);

            // ureq v2 returns non-2xx as Err(ureq::Error::Status(code, resp)).
            // All match arms must be on the Err side for non-2xx status codes.
            match result {
                Ok(resp) => {
                    let status = resp.status();
                    let body = resp.into_string().unwrap_or_default();
                    if !body.is_empty() {
                        let redacted = self.redact(&body);
                        log_info!("[transport] HTTP {status} response body: {redacted}");
                    }
                    return Ok(parse_receipt(status, &body, event_count));
                }

                Err(ureq::Error::Status(401, resp)) => {
                    let body = self.redact(&resp.into_string().unwrap_or_default());
                    log_warn!("[transport] 401 response body: {body}");

                    // A static local token cannot be refreshed — 401 is final.
                    if matches!(self.credential, Credential::Static(_)) {
                        return Err(TransportError::new(
                            TransportErrorKind::Auth,
                            "401 unauthorized — local token rejected",
                        ));
                    }

                    // Stale token — attempt one forced refresh on the first 401, then fatal.
                    if attempt == 0 {
                        log_warn!("[transport] 401 — refreshing token and retrying…");
                        if let Credential::Entra { cached_token, .. } = &mut self.credential {
                            *cached_token = None;
                        }
                        let token = self.get_or_refresh_token(true)?;
                        let result2 = self
                            .agent
                            .post(&self.ingest_url)
                            .set("Authorization", &format!("Bearer {token}"))
                            .set("Content-Type", "application/json")
                            .send_json(body_value);
                        return match result2 {
                            Ok(r2) => {
                                let status2 = r2.status();
                                let body2 = r2.into_string().unwrap_or_default();
                                if !body2.is_empty() {
                                    let redacted2 = self.redact(&body2);
                                    log_info!(
                                        "[transport] HTTP {status2} response body: {redacted2}"
                                    );
                                }
                                Ok(parse_receipt(status2, &body2, event_count))
                            }
                            Err(ureq::Error::Status(code, r2)) => {
                                let body2 = self.redact(&r2.into_string().unwrap_or_default());
                                log_warn!("[transport] HTTP {code} response body: {body2}");
                                let kind = match code {
                                    401 | 403 => TransportErrorKind::Auth,
                                    _ => TransportErrorKind::Http(code),
                                };
                                Err(TransportError::new(
                                    kind,
                                    format!("HTTP {code} after forced token refresh — fatal"),
                                ))
                            }
                            Err(e) => {
                                let kind = classify_ureq_error(&e);
                                Err(TransportError::new(
                                    kind,
                                    self.redact(&format!("network error after token refresh: {e}")),
                                ))
                            }
                        };
                    }
                    return Err(TransportError::new(
                        TransportErrorKind::Auth,
                        "401 after forced token refresh — fatal",
                    ));
                }

                Err(ureq::Error::Status(code, resp)) => {
                    let body = self.redact(&resp.into_string().unwrap_or_default());
                    last_kind = match code {
                        401 | 403 => TransportErrorKind::Auth,
                        _ => TransportErrorKind::Http(code),
                    };
                    last_error = format!("HTTP {code}");
                    log_warn!(
                        "[transport] {} attempt {}/{} — body: {}",
                        last_error,
                        attempt + 1,
                        total_attempts,
                        body,
                    );
                }

                Err(e) => {
                    last_kind = classify_ureq_error(&e);
                    last_error = self.redact(&format!("network error: {e}"));
                    log_warn!(
                        "[transport] {} attempt {}/{}, retrying…",
                        last_error,
                        attempt + 1,
                        total_attempts
                    );
                }
            }
        }

        Err(TransportError::new(
            last_kind,
            format!("max retries exceeded: {last_error}"),
        ))
    }

    /// Replace any occurrence of a credential (static token, client secret,
    /// cached bearer token) in `s` with `[REDACTED]`.
    fn redact(&self, s: &str) -> String {
        let secrets: Vec<&str> = match &self.credential {
            Credential::Static(token) => vec![token.as_str()],
            Credential::Entra {
                client_secret,
                cached_token,
                ..
            } => {
                let mut v = vec![client_secret.as_str()];
                if let Some(t) = cached_token {
                    v.push(t.token.as_str());
                }
                v
            }
        };
        Self::redact_with(&secrets, s)
    }

    fn redact_with(secrets: &[&str], s: &str) -> String {
        let mut out = s.to_owned();
        for secret in secrets {
            if !secret.is_empty() {
                out = out.replace(secret, "[REDACTED]");
            }
        }
        out
    }

    fn get_or_refresh_token(&mut self, force: bool) -> Result<String, TransportError> {
        match &mut self.credential {
            Credential::Static(token) => Ok(token.clone()),
            Credential::Entra {
                client_id,
                client_secret,
                scope,
                token_url,
                cached_token,
            } => {
                let needs_refresh = force
                    || match cached_token {
                        None => true,
                        Some(t) => {
                            let buffer = Duration::from_secs(TOKEN_REFRESH_BUFFER_S);
                            t.expires_at <= SystemTime::now() + buffer
                        }
                    };

                if needs_refresh {
                    log_info!("[transport] acquiring token from {token_url}");
                    let resp = Self::fetch_token(
                        &self.agent,
                        &[client_secret.as_str()],
                        client_id,
                        client_secret,
                        scope,
                        token_url,
                    )?;
                    let expires_in = resp.expires_in;
                    let expires_at = SystemTime::now()
                        + Duration::from_secs(expires_in.saturating_sub(TOKEN_REFRESH_BUFFER_S));
                    log_info!("[transport] token acquired — expires_in={expires_in}s");
                    *cached_token = Some(CachedToken {
                        token: resp.access_token,
                        expires_at,
                    });
                }

                Ok(cached_token.as_ref().unwrap().token.clone())
            }
        }
    }

    fn fetch_token(
        agent: &ureq::Agent,
        secrets: &[&str],
        client_id: &str,
        client_secret: &str,
        scope: &str,
        token_url: &str,
    ) -> Result<TokenResponse, TransportError> {
        log_info!(
            "[transport] POST {} client_id={}…{}",
            token_url,
            &client_id[..8.min(client_id.len())],
            &client_id[client_id.len().saturating_sub(4)..]
        );
        let result = agent
            .post(token_url)
            .set("Content-Type", "application/x-www-form-urlencoded")
            .send_form(&[
                ("grant_type", "client_credentials"),
                ("client_id", client_id),
                ("client_secret", client_secret),
                ("scope", scope),
            ]);

        let resp = match result {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                let body = Self::redact_with(secrets, &r.into_string().unwrap_or_default());
                return Err(TransportError::new(
                    match code {
                        401 | 403 => TransportErrorKind::Auth,
                        _ => TransportErrorKind::Http(code),
                    },
                    format!("token request failed: status {code} — {body}"),
                ));
            }
            Err(e) => {
                return Err(TransportError::new(
                    classify_ureq_error(&e),
                    Self::redact_with(secrets, &format!("token request failed: {e}")),
                ));
            }
        };

        let token_resp: TokenResponse = resp.into_json().map_err(|e| {
            TransportError::new(
                TransportErrorKind::Config,
                format!("token response parse error: {e}"),
            )
        })?;

        Ok(token_resp)
    }

    /// Wall-clock time at which the cached token expires, or `None` if no
    /// token has been acquired yet. Used by the UI status display.
    /// Always `None` for static-token (local) credentials.
    pub fn token_expires_at(&self) -> Option<SystemTime> {
        match &self.credential {
            Credential::Entra { cached_token, .. } => cached_token.as_ref().map(|t| t.expires_at),
            Credential::Static(_) => None,
        }
    }

    /// Inject a pre-built token, bypassing network calls. **Test use only.**
    #[cfg(test)]
    pub fn set_token_for_test(&mut self, token: &str) {
        if let Credential::Entra { cached_token, .. } = &mut self.credential {
            *cached_token = Some(CachedToken {
                token: token.to_owned(),
                expires_at: SystemTime::now() + Duration::from_secs(3_600),
            });
        }
    }

    /// Override the ingest URL. **Test use only.**
    #[cfg(test)]
    pub fn set_ingest_url_for_test(&mut self, url: &str) {
        self.ingest_url = url.to_owned();
    }

    /// Override the Entra token URL. **Test use only.**
    #[cfg(test)]
    pub fn set_token_url_for_test(&mut self, url: &str) {
        if let Credential::Entra { token_url, .. } = &mut self.credential {
            *token_url = url.to_owned();
        }
    }

    /// Replace the retry delay schedule (initial attempt still fires).
    /// Delivery uses an empty schedule for the bounded shutdown drain; tests
    /// use it to keep retry behavior deterministic.
    pub fn set_retry_delays(&mut self, delays: &[u64]) {
        self.retry_delays = delays.to_vec();
    }

    /// Replace the retry delay schedule (initial attempt still fires).
    /// **Test use only.**
    #[cfg(test)]
    pub fn set_retry_delays_for_test(&mut self, delays: &[u64]) {
        self.set_retry_delays(delays);
    }
}

/// Parse a 2xx ingest response body into a [`BatchReceipt`]. When the body is
/// empty or unparseable, `accepted` falls back to `batch_size` — the HTTP
/// acknowledgement alone covers the whole batch.
fn parse_receipt(status: u16, body: &str, batch_size: usize) -> BatchReceipt {
    let parsed: Option<serde_json::Value> = serde_json::from_str(body).ok();
    let Some(value) = parsed else {
        return BatchReceipt {
            http_status: status,
            accepted: batch_size,
            ..BatchReceipt::default()
        };
    };
    let count = |key: &str| value.get(key).and_then(|v| v.as_u64()).map(|n| n as usize);
    BatchReceipt {
        http_status: status,
        accepted: count("accepted").unwrap_or(batch_size),
        rejected: count("rejected").unwrap_or(0),
        duplicate: count("duplicate").unwrap_or(0),
        spooled: value
            .get("spooled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    }
}

// ── Wire types ────────────────────────────────────────────────────────────────

/// Outer envelope for `POST /api/publisher/v2/ingest`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct IngestRequest<'a> {
    sub_session_id: i64,
    session_time: f64,
    session_tick: i64,
    events: &'a [PublisherEvent],
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    /// Lifetime in seconds from time of issuance.
    expires_in: u64,
}

struct CachedToken {
    token: String,
    /// Wall-clock expiry, already adjusted by [`TOKEN_REFRESH_BUFFER_S`].
    expires_at: SystemTime,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::publisher_event::build_event;
    use crate::race_event::{LifecycleOrigin, RaceEvent};
    use crate::telemetry_frame::TelemetryFrame;

    fn minimal_frame() -> TelemetryFrame {
        TelemetryFrame {
            lap: 1,
            session_time: 10.0,
            lap_dist_pct: 0.5,
            player_car_idx: 0,
            player_car_position: 5,
            on_pit_road: false,
            session_flags: 0,
            car_idx_lap_dist_pct: vec![0.5],
            car_idx_position: vec![5],
            car_idx_on_pit_road: vec![false],
            car_idx_track_surface: vec![0],
            lap_last_lap_time: 540.0,
            session_info_update: 1,
            session_tick: 100,
            session_state: 4,
            session_num: 0,
            session_time_remain: None,
            session_laps_remain: None,
            player_incident_count: 0,
            car_idx_lap_completed: vec![1],
            lf_temp_m: 0.0,
            rf_temp_m: 0.0,
            lr_temp_m: 0.0,
            rr_temp_m: 0.0,
            fuel_level: 0.0,
            throttle: 0.0,
            brake: 0.0,
            speed: 0.0,
        }
    }

    fn make_transport(server_url: &str) -> PublisherTransport {
        let mut t = PublisherTransport::new("tenant", "client", "secret", "scope", server_url, 500);
        t.set_token_for_test("test-bearer-token");
        t
    }

    #[test]
    fn post_batch_includes_auth_header_and_json_shape() {
        let mut server = mockito::Server::new();

        // Expect one POST with the correct Authorization header
        let mock = server
            .mock("POST", "/api/publisher/v2/ingest")
            .with_status(200)
            .with_body("{}")
            .match_header(
                "authorization",
                mockito::Matcher::Exact("Bearer test-bearer-token".to_owned()),
            )
            .match_header(
                "content-type",
                mockito::Matcher::Regex("application/json".to_owned()),
            )
            .create();

        let event = build_event(
            &RaceEvent::RaceGreen {
                lap: 1,
                session_time: 10.0,
                synthetic: false,
                origin: LifecycleOrigin::SessionStateTransition,
            },
            &minimal_frame(),
            None,
            "session-xyz",
            "rig-001",
            None,
            None,
        );

        let mut transport = make_transport(&server.url());
        transport.post_events(&[event], 10.0, 100, 99999).unwrap();

        mock.assert();
    }

    #[test]
    fn ingest_request_serialises_to_expected_shape() {
        let event = build_event(
            &RaceEvent::RaceGreen {
                lap: 1,
                session_time: 10.0,
                synthetic: false,
                origin: LifecycleOrigin::SessionStateTransition,
            },
            &minimal_frame(),
            None,
            "session-xyz",
            "rig-001",
            None,
            None,
        );

        let req = IngestRequest {
            sub_session_id: 99999,
            session_time: 10.0,
            session_tick: 100,
            events: &[event],
        };

        let json: serde_json::Value = serde_json::to_value(&req).unwrap();
        assert_eq!(json["subSessionId"], 99999_i64);
        assert_eq!(json["sessionTime"], 10.0_f64);
        assert_eq!(json["sessionTick"], 100_i64);
        assert!(json["events"].is_array());
        assert_eq!(json["events"].as_array().unwrap().len(), 1);
        assert_eq!(json["events"][0]["type"], "RACE_GREEN");
    }

    #[test]
    fn token_is_reused_across_calls() {
        let mut server = mockito::Server::new();
        server
            .mock("POST", "/api/publisher/v2/ingest")
            .with_status(200)
            .with_body("{}")
            .expect(2) // two flush calls
            .create();

        let mut transport = make_transport(&server.url());
        let frame = minimal_frame();

        for _ in 0..2 {
            let event = build_event(
                &RaceEvent::RaceGreen {
                    lap: 1,
                    session_time: 10.0,
                    synthetic: false,
                    origin: LifecycleOrigin::SessionStateTransition,
                },
                &frame,
                None,
                "s",
                "r",
                None,
                None,
            );
            transport.post_events(&[event], 10.0, 100, 1).unwrap();
        }

        // No token fetch calls (token was injected) — if token refresh were
        // triggered unexpectedly, the fetch_token() call to the non-mocked
        // Azure endpoint would fail and the test would error.
    }

    #[test]
    fn warmup_auth_succeeds_with_cached_token() {
        let mut transport = make_transport("http://localhost");
        transport
            .warmup_auth()
            .expect("warmup should use cached token");
        assert!(transport.token_expires_at().is_some());
    }

    // ── Local destination ─────────────────────────────────────────────────

    fn race_green_event() -> PublisherEvent {
        build_event(
            &RaceEvent::RaceGreen {
                lap: 1,
                session_time: 10.0,
                synthetic: false,
                origin: LifecycleOrigin::SessionStateTransition,
            },
            &minimal_frame(),
            None,
            "session-xyz",
            "rig-001",
            None,
            None,
        )
    }

    #[test]
    fn local_destination_posts_bearer_and_unchanged_body() {
        let mut server = mockito::Server::new();

        let mock = server
            .mock("POST", "/api/publisher/v2/ingest")
            .with_status(202)
            .with_body("{}")
            .match_header(
                "authorization",
                mockito::Matcher::Exact("Bearer local-token".to_owned()),
            )
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({
                "subSessionId": 99999,
                "sessionTick":  100,
                "events": [{"type": "RACE_GREEN"}],
            })))
            .expect(1)
            .create();

        let mut transport =
            PublisherTransport::new_local(&server.url(), "local-token", None, 500).unwrap();
        transport
            .post_events(&[race_green_event()], 10.0, 100, 99999)
            .unwrap();

        mock.assert();
        assert!(transport.token_expires_at().is_none());
    }

    #[test]
    fn dry_run_performs_no_network_calls() {
        let mut server = mockito::Server::new();
        let ingest_mock = server
            .mock("POST", "/api/publisher/v2/ingest")
            .expect(0)
            .create();
        let token_mock = server.mock("POST", "/token").expect(0).create();

        // Entra transport WITHOUT an injected token — any network call would
        // hit these mocks and be counted.
        let mut transport =
            PublisherTransport::new("tenant", "client", "secret", "scope", server.url(), 500);
        transport.set_token_url_for_test(&format!("{}/token", server.url()));
        transport.set_dry_run(true);

        transport.warmup_auth().unwrap();
        transport
            .post_events(&[race_green_event()], 10.0, 100, 99999)
            .unwrap();

        ingest_mock.assert();
        token_mock.assert();
        assert!(transport.token_expires_at().is_none());
    }

    #[test]
    fn static_token_401_is_fatal_without_refresh() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/api/publisher/v2/ingest")
            .with_status(401)
            .with_body("denied")
            .expect(1)
            .create();

        let mut transport =
            PublisherTransport::new_local(&server.url(), "local-token", None, 500).unwrap();
        let err = transport
            .post_events(&[race_green_event()], 10.0, 100, 99999)
            .unwrap_err();

        assert_eq!(err.kind, TransportErrorKind::Auth);
        mock.assert();
    }

    #[test]
    fn errors_never_contain_token() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/api/publisher/v2/ingest")
            .with_status(500)
            .with_body("server exploded processing local-token")
            .expect(4) // 1 initial + 3 retries
            .create();

        let mut transport =
            PublisherTransport::new_local(&server.url(), "local-token", None, 500).unwrap();
        let err = transport
            .post_events(&[race_green_event()], 10.0, 100, 99999)
            .unwrap_err();

        mock.assert();
        assert_eq!(err.kind, TransportErrorKind::Http(500));
        assert!(
            !err.message.contains("local-token"),
            "error message leaked the token: {}",
            err.message
        );
    }

    // ── TLS pinning (live rustls server) ──────────────────────────────────

    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    struct TlsTestServer {
        port: u16,
        rx: mpsc::Receiver<String>,
    }

    /// Spawn a one-shot-per-connection TLS server that replies 202 to any
    /// request and forwards the raw request text over the channel.
    fn spawn_tls_server(
        cert_der: rustls::pki_types::CertificateDer<'static>,
        key_der: rustls::pki_types::PrivateKeyDer<'static>,
    ) -> TlsTestServer {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();

        std::thread::spawn(move || {
            let config = rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![cert_der], key_der)
                .unwrap();
            let config = std::sync::Arc::new(config);

            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let conn = match rustls::ServerConnection::new(config.clone()) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let mut tls = rustls::StreamOwned::new(conn, stream);
                let tx = tx.clone();
                std::thread::spawn(move || {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 8192];
                    // Handshake + read. On pin-mismatch the client aborts the
                    // handshake, so reads fail — nothing is forwarded.
                    let request: Option<String> = loop {
                        match tls.read(&mut chunk) {
                            Ok(0) | Err(_) => break None,
                            Ok(n) => {
                                buf.extend_from_slice(&chunk[..n]);
                                if let Some(req) = complete_request(&buf) {
                                    break Some(req);
                                }
                                if buf.len() > 1 << 20 {
                                    break None;
                                }
                            }
                        }
                    };
                    if let Some(text) = request {
                        let _ = tx.send(text);
                        let _ = tls.write_all(
                            b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                    }
                });
            }
        });

        TlsTestServer { port, rx }
    }

    /// Return the request text once headers + Content-Length body are complete.
    fn complete_request(buf: &[u8]) -> Option<String> {
        let text = String::from_utf8_lossy(buf).to_string();
        let head_end = text.find("\r\n\r\n")?;
        let content_length: usize = text[..head_end]
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                if k.trim().eq_ignore_ascii_case("content-length") {
                    v.trim().parse().ok()
                } else {
                    None
                }
            })
            .unwrap_or(0);
        (buf.len() >= head_end + 4 + content_length).then_some(text)
    }

    fn self_signed_cert(
        names: &[&str],
    ) -> (
        rustls::pki_types::CertificateDer<'static>,
        rustls::pki_types::PrivateKeyDer<'static>,
    ) {
        let rcgen::CertifiedKey { cert, key_pair } = rcgen::generate_simple_self_signed(
            names.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        )
        .unwrap();
        let key = rustls::pki_types::PrivatePkcs8KeyDer::from(key_pair.serialize_der());
        (cert.der().clone(), key.into())
    }

    #[test]
    fn tls_pinned_local_accept() {
        let (cert_der, key_der) = self_signed_cert(&["localhost"]);
        let fingerprint = crate::tls_pin::sha256_hex(cert_der.as_ref());
        let server = spawn_tls_server(cert_der, key_der);

        let url = format!("https://127.0.0.1:{}", server.port);
        let mut transport =
            PublisherTransport::new_local(&url, "tok", Some(&fingerprint), 500).unwrap();
        transport
            .post_events(&[race_green_event()], 10.0, 100, 99999)
            .unwrap();

        let request = server
            .rx
            .recv_timeout(Duration::from_secs(5))
            .expect("server should have received the request");

        let first_line = request.lines().next().unwrap();
        assert_eq!(first_line, "POST /api/publisher/v2/ingest HTTP/1.1");
        let auth = request
            .lines()
            .find(|l| l.to_lowercase().starts_with("authorization:"))
            .expect("authorization header");
        assert_eq!(auth.trim(), "Authorization: Bearer tok");

        let body = &request[request.find("\r\n\r\n").unwrap() + 4..];
        let json: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(json["subSessionId"], 99999);
        assert!(json["sessionTime"].is_f64());
        assert_eq!(json["sessionTick"], 100);
        assert_eq!(json["events"][0]["type"], "RACE_GREEN");
    }

    #[test]
    fn tls_pinned_local_reject_wrong_fingerprint() {
        let (cert_der, key_der) = self_signed_cert(&["localhost"]);
        let (other_cert_der, _other_key) = self_signed_cert(&["localhost"]);
        let wrong_fingerprint = crate::tls_pin::sha256_hex(other_cert_der.as_ref());
        let server = spawn_tls_server(cert_der, key_der);

        let url = format!("https://127.0.0.1:{}", server.port);
        let mut transport =
            PublisherTransport::new_local(&url, "tok", Some(&wrong_fingerprint), 500).unwrap();
        transport.set_retry_delays_for_test(&[]); // single attempt
        let err = transport
            .post_events(&[race_green_event()], 10.0, 100, 99999)
            .unwrap_err();

        assert!(
            matches!(
                err.kind,
                TransportErrorKind::Tls | TransportErrorKind::Network
            ),
            "expected TLS/Network failure, got {:?}: {}",
            err.kind,
            err.message
        );
        assert!(
            server.rx.recv_timeout(Duration::from_secs(2)).is_err(),
            "server must not receive a request when the pin mismatches"
        );
    }
}
