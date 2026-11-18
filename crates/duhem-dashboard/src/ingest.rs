//! Opt-in HTTP replication endpoint. Read handlers retain a read-only handle.
use std::sync::Arc;

use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use duhem_evidence::{RunBundle, SqliteStore, store::IngestError};

pub const MAX_BUNDLE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone)]
struct IngestState {
    store: Arc<SqliteStore>,
    token: Option<String>,
}

/// Merge this router only when the operator explicitly enables ingest.
pub fn router(store: Arc<SqliteStore>, token: Option<String>) -> Router {
    Router::new()
        .route("/api/ingest", post(ingest))
        .layer(DefaultBodyLimit::max(MAX_BUNDLE_BYTES))
        .with_state(IngestState {
            store,
            token: token.filter(|value| !value.is_empty()),
        })
}

async fn ingest(State(state): State<IngestState>, headers: HeaderMap, body: Bytes) -> Response {
    if let Some(token) = &state.token {
        let expected = format!("Bearer {token}");
        if headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            != Some(expected.as_str())
        {
            return (StatusCode::UNAUTHORIZED, "bearer token required").into_response();
        }
    }
    if headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        != Some("application/json")
        || headers
            .get("x-duhem-bundle-version")
            .and_then(|v| v.to_str().ok())
            != Some("1")
    {
        return (
            StatusCode::BAD_REQUEST,
            "unsupported content type or bundle version",
        )
            .into_response();
    }
    let Some(hash) = headers
        .get("x-duhem-content-hash")
        .and_then(|v| v.to_str().ok())
    else {
        return (StatusCode::BAD_REQUEST, "missing content hash").into_response();
    };
    let bundle: RunBundle = match serde_json::from_slice(&body) {
        Ok(bundle) => bundle,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid bundle JSON").into_response(),
    };
    match state.store.ingest_bundle(&bundle, hash).await {
        Ok(_) => StatusCode::OK.into_response(),
        Err(IngestError::Invalid(error)) => (StatusCode::BAD_REQUEST, error).into_response(),
        Err(IngestError::Conflict) => (StatusCode::CONFLICT, "conflicting run id").into_response(),
        Err(error) => {
            eprintln!("ingest: {error}");
            (StatusCode::INTERNAL_SERVER_ERROR, "could not commit bundle").into_response()
        }
    }
}
