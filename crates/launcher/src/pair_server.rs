// SPDX-License-Identifier: GPL-3.0-or-later

use std::sync::Arc;

use axum::{
    body::{to_bytes, Body},
    extract::State,
    http::{header, Request, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;

use crate::{
    pairing::{DirectorInfo, PairError, PairRequest, PairingState},
    runner::SwappableHandler,
};

const MAX_PAIR_BODY: usize = 16 * 1024;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PairBody {
    pairing_code: String,
    director: DirectorBody,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DirectorBody {
    name: String,
    ingest_url: String,
    ingest_fingerprint: String,
}

pub fn build_publisher_router(
    handler: Arc<SwappableHandler>,
    pairing: Arc<PairingState>,
) -> Router {
    let protected =
        mcp_core::transport::http::build_protected_router(handler, pairing.credentials.clone());
    let pair_router = Router::new().route("/pair", post(pair)).with_state(pairing);
    protected.merge(pair_router)
}

async fn pair(State(pairing): State<Arc<PairingState>>, request: Request<Body>) -> Response {
    if request.headers().contains_key(header::ORIGIN) {
        return error_response(StatusCode::FORBIDDEN, "origin_forbidden");
    }
    if request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|length| length > MAX_PAIR_BODY)
    {
        return error_response(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large");
    }
    let body = match to_bytes(request.into_body(), MAX_PAIR_BODY).await {
        Ok(body) => body,
        Err(_) => return error_response(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large"),
    };
    let parsed: PairBody = match serde_json::from_slice(&body) {
        Ok(parsed) => parsed,
        Err(_) => return error_response(StatusCode::UNPROCESSABLE_ENTITY, "schema"),
    };
    if parsed.pairing_code.len() != 3
        || !parsed
            .pairing_code
            .bytes()
            .all(|byte| byte.is_ascii_digit())
    {
        return error_response(StatusCode::FORBIDDEN, "invalid_pairing_code");
    }
    let request = PairRequest {
        pairing_code: parsed.pairing_code,
        director: DirectorInfo {
            name: parsed.director.name,
            ingest_url: parsed.director.ingest_url,
            ingest_fingerprint: parsed.director.ingest_fingerprint,
        },
    };
    match pairing.pair(request) {
        Ok(response) => (StatusCode::OK, Json(response)).into_response(),
        Err(PairError::InvalidCode) => {
            error_response(StatusCode::FORBIDDEN, "invalid_pairing_code")
        }
        Err(PairError::AlreadyPaired) => error_response(StatusCode::CONFLICT, "already_paired"),
        Err(PairError::RateLimited { retry_after_secs }) => (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, retry_after_secs.to_string())],
            Json(json!({"error": "rate_limited"})),
        )
            .into_response(),
        Err(PairError::Internal(error)) => {
            tracing::error!(%error, "publisher pairing failed");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
        }
    }
}

fn error_response(status: StatusCode, error: &'static str) -> Response {
    (status, Json(json!({"error": error}))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::LauncherConfig,
        pairing::{InMemoryPairingStore, PUBLISHER_SCOPE},
        runner::build_handler,
    };
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn setup() -> (Router, Arc<PairingState>) {
        let store = Arc::new(InMemoryPairingStore::new(LauncherConfig::default()));
        let pairing = Arc::new(
            PairingState::new(
                Arc::new(mcp_core::transport::http::access::CredentialRegistry::new()),
                store,
                "rig1".to_string(),
                "AA:BB".to_string(),
            )
            .unwrap(),
        );
        (
            build_publisher_router(
                Arc::new(crate::runner::SwappableHandler::new(build_handler(
                    crate::config::Sim::Publisher,
                ))),
                pairing.clone(),
            ),
            pairing,
        )
    }

    fn pair_request(code: &str) -> Request<Body> {
        Request::post("/pair")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({
                    "pairingCode": code,
                    "director": {
                        "name": "Director",
                        "ingestUrl": "https://director.example/ingest",
                        "ingestFingerprint": "AA:BB"
                    }
                })
                .to_string(),
            ))
            .unwrap()
    }

    #[tokio::test]
    async fn pair_is_public_but_mcp_requires_bearer() {
        let (app, pairing) = setup();
        let response = app
            .clone()
            .oneshot(pair_request(&pairing.pairing_code()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let credential = body["credential"].as_str().unwrap();
        assert_eq!(body["expiresAt"], serde_json::Value::Null);

        let unauthorized = app
            .clone()
            .oneshot(
                Request::post("/mcp")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let authorized = app
            .oneshot(
                Request::post("/mcp")
                    .header(header::AUTHORIZATION, format!("Bearer {credential}"))
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(authorized.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn origin_is_rejected_before_body_read() {
        let (app, _) = setup();
        let body = Body::from_stream(tokio_stream::once(
            Err::<axum::body::Bytes, std::io::Error>(std::io::Error::other("body read")),
        ));
        let response = app
            .oneshot(
                Request::post("/pair")
                    .header(header::ORIGIN, "https://director.example")
                    .body(body)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            json!({"error": "origin_forbidden"})
        );
    }

    #[tokio::test]
    async fn wrong_codes_lock_pairing_and_correct_code_stays_locked() {
        let (app, pairing) = setup();
        let wrong = if pairing.pairing_code() == "000" {
            "001"
        } else {
            "000"
        };
        for _ in 0..5 {
            let response = app.clone().oneshot(pair_request(wrong)).await.unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
        let response = app.clone().oneshot(pair_request(wrong)).await.unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[header::RETRY_AFTER], "60");
        let response = app
            .oneshot(pair_request(&pairing.pairing_code()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn pair_rejects_oversize_and_malformed_requests() {
        let (app, _) = setup();
        let response = app
            .clone()
            .oneshot(
                Request::post("/pair")
                    .header(header::CONTENT_LENGTH, MAX_PAIR_BODY + 1)
                    .body(Body::from(vec![b'x'; MAX_PAIR_BODY + 1]))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let response = app
            .oneshot(
                Request::post("/pair")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"pairingCode":"123"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn restored_pairing_accepts_old_credential_until_unpair() {
        let store = Arc::new(InMemoryPairingStore::new(LauncherConfig::default()));
        let first_pairing = Arc::new(
            PairingState::new(
                Arc::new(mcp_core::transport::http::access::CredentialRegistry::new()),
                store.clone(),
                "rig1".to_string(),
                "AA:BB".to_string(),
            )
            .unwrap(),
        );
        let credential = {
            let response = first_pairing
                .pair(crate::pairing::PairRequest {
                    pairing_code: first_pairing.pairing_code(),
                    director: crate::pairing::DirectorInfo {
                        name: "Director".to_string(),
                        ingest_url: "https://director.example/ingest".to_string(),
                        ingest_fingerprint: "AA:BB".to_string(),
                    },
                })
                .unwrap();
            response.credential
        };
        let second_pairing = Arc::new(
            PairingState::new(
                Arc::new(mcp_core::transport::http::access::CredentialRegistry::new()),
                store,
                "rig1".to_string(),
                "AA:BB".to_string(),
            )
            .unwrap(),
        );
        let app = build_publisher_router(
            Arc::new(crate::runner::SwappableHandler::new(build_handler(
                crate::config::Sim::Publisher,
            ))),
            second_pairing.clone(),
        );
        let response = app
            .clone()
            .oneshot(
                Request::post("/mcp")
                    .header(header::AUTHORIZATION, format!("Bearer {credential}"))
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        second_pairing.unpair().unwrap();
        let response = app
            .oneshot(
                Request::post("/mcp")
                    .header(header::AUTHORIZATION, format!("Bearer {credential}"))
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":2,"method":"initialize"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn publisher_scope_matches_advertised_publisher_tools() {
        let mut scope: Vec<&str> = PUBLISHER_SCOPE.to_vec();
        let mut advertised: Vec<&str> = publisher_mcp::MUTATING_TOOLS
            .iter()
            .chain(publisher_mcp::READ_ONLY_TOOLS)
            .copied()
            .collect();
        scope.sort_unstable();
        advertised.sort_unstable();
        assert_eq!(scope, advertised);
    }
}
