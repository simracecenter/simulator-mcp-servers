use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    body::{to_bytes, Body},
    extract::{Request, State},
    http::{header, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use sha2::{Digest, Sha256};
use tokio::time::Instant;
use uuid::Uuid;

use crate::jsonrpc::JsonRpcRequest;

use super::SESSION_HEADER;

const MAX_CREDENTIALS: usize = 64;
const MAX_BODY_BYTES: usize = 1024 * 1024;

pub struct IssuedCredential {
    id: Uuid,
    token: String,
}

impl IssuedCredential {
    pub fn id(&self) -> Uuid {
        self.id
    }

    pub fn token(&self) -> &str {
        &self.token
    }
}

#[derive(Clone)]
struct Grant {
    id: Uuid,
    tools: HashSet<String>,
    expires_at: Instant,
}

impl Grant {
    fn permits(&self, request: &JsonRpcRequest) -> bool {
        match request.method.as_str() {
            "initialize" | "ping" | "tools/list" => request.id.is_some(),
            "notifications/initialized" => request.id.is_none(),
            "tools/call" => {
                request.id.is_some()
                    && request
                        .params
                        .get("name")
                        .and_then(|name| name.as_str())
                        .is_some_and(|name| self.tools.contains(name))
            }
            _ => false,
        }
    }
}

#[derive(Default)]
pub struct CredentialRegistry {
    grants: Mutex<HashMap<[u8; 32], Grant>>,
}

impl CredentialRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn issue(
        &self,
        tools: impl IntoIterator<Item = impl Into<String>>,
        lifetime: Duration,
    ) -> Result<IssuedCredential, &'static str> {
        if lifetime.is_zero() || lifetime > Duration::from_secs(86400) {
            return Err("credential lifetime must be within 24 hours");
        }
        let tools: HashSet<String> = tools.into_iter().map(Into::into).collect();
        if tools.len() > 256
            || tools
                .iter()
                .any(|tool| tool.is_empty() || tool.len() > 200 || tool == "*")
        {
            return Err("invalid tool allowlist");
        }
        let now = Instant::now();
        let mut grants = self.grants.lock().expect("credential registry");
        grants.retain(|_, grant| grant.expires_at > now);
        if grants.len() >= MAX_CREDENTIALS {
            return Err("credential capacity reached");
        }
        let credential = IssuedCredential {
            id: Uuid::new_v4(),
            token: format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
        };
        grants.insert(
            Sha256::digest(credential.token.as_bytes()).into(),
            Grant {
                id: credential.id,
                tools,
                expires_at: now + lifetime,
            },
        );
        Ok(credential)
    }

    pub fn revoke(&self, id: Uuid) {
        self.grants
            .lock()
            .expect("credential registry")
            .retain(|_, grant| grant.id != id);
    }

    fn authenticate(&self, token: &str) -> Option<Grant> {
        if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        self.grants
            .lock()
            .expect("credential registry")
            .get(&digest)
            .filter(|grant| grant.expires_at > Instant::now())
            .cloned()
    }
}

pub(super) struct AccessState {
    credentials: Arc<CredentialRegistry>,
    owners: Mutex<HashMap<String, Uuid>>,
}

impl AccessState {
    pub(super) fn new(credentials: Arc<CredentialRegistry>) -> Self {
        Self {
            credentials,
            owners: Mutex::new(HashMap::new()),
        }
    }
}

pub(super) async fn authorize(
    State(state): State<Arc<AccessState>>,
    mut request: Request,
    next: Next,
) -> Response {
    if request.headers().contains_key(header::ORIGIN) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let authorization = request.headers().get_all(header::AUTHORIZATION);
    if authorization.iter().count() != 1 {
        return unauthorized();
    }
    let Some(grant) = authorization
        .iter()
        .next()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .and_then(|(_, token)| state.credentials.authenticate(token))
    else {
        return unauthorized();
    };
    let session_headers = request.headers().get_all(SESSION_HEADER);
    if session_headers.iter().count() > 1 {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let session = match session_headers.iter().next() {
        Some(value) => match value.to_str() {
            Ok(value) => Some(value.to_owned()),
            Err(_) => return StatusCode::BAD_REQUEST.into_response(),
        },
        None => None,
    };
    if let Some(session) = &session {
        if state.owners.lock().expect("session owners").get(session) != Some(&grant.id) {
            return super::unknown_session();
        }
    }
    let method = request.method().clone();
    let is_mcp = request.uri().path() == "/mcp";
    if is_mcp && method == Method::POST {
        let (parts, body) = request.into_parts();
        let bytes = match to_bytes(body, MAX_BODY_BYTES).await {
            Ok(bytes) => bytes,
            Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
        };
        let parsed: JsonRpcRequest = match serde_json::from_slice(&bytes) {
            Ok(parsed) => parsed,
            Err(_) => return StatusCode::BAD_REQUEST.into_response(),
        };
        if !grant.permits(&parsed) {
            return StatusCode::FORBIDDEN.into_response();
        }
        if parsed.method == "initialize" {
            if session.is_some() {
                return StatusCode::BAD_REQUEST.into_response();
            }
        } else if session.is_none() {
            return StatusCode::BAD_REQUEST.into_response();
        }
        request = Request::from_parts(parts, Body::from(bytes));
    } else if is_mcp && session.is_none() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if !state
        .credentials
        .grants
        .lock()
        .expect("credential registry")
        .values()
        .any(|current| current.id == grant.id && current.expires_at > Instant::now())
    {
        return unauthorized();
    }
    let response = next.run(request).await;
    if let Some(session) = response
        .headers()
        .get(SESSION_HEADER)
        .and_then(|value| value.to_str().ok())
    {
        state
            .owners
            .lock()
            .expect("session owners")
            .insert(session.to_owned(), grant.id);
    }
    if method == Method::DELETE && response.status() == StatusCode::NO_CONTENT {
        if let Some(session) = session {
            state
                .owners
                .lock()
                .expect("session owners")
                .remove(&session);
        }
    }
    response
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
    )
        .into_response()
}
