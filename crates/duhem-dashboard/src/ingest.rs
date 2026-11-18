use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, FromRequest, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use duhem_evidence::{BUNDLE_VERSION, SqliteStore, store::IngestError};

const BODY_LIMIT: usize = 16 * 1024 * 1024;

#[derive(Clone)]
struct Ingest {
    store: Arc<SqliteStore>,
    token: Option<Arc<str>>,
}

/// Enable writes explicitly. The caller enforces the token requirement for
/// the bound address; the ordinary dashboard router has no ingest route.
pub fn ingest_router(store: Arc<SqliteStore>, token: Option<Arc<str>>) -> Router {
    Router::new()
        .route("/api/ingest", post(ingest))
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .with_state(Ingest { store, token })
}

fn token_matches(actual: &[u8], expected: &[u8]) -> bool {
    actual.len() == expected.len()
        && actual
            .iter()
            .zip(expected)
            .fold(0, |diff, (a, b)| diff | (a ^ b))
            == 0
}

async fn ingest(State(state): State<Ingest>, request: Request) -> Response {
    let headers = request.headers();
    if let Some(token) = &state.token
        && !headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.as_bytes().strip_prefix(b"Bearer "))
            .is_some_and(|value| token_matches(value, token.as_bytes()))
    {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
        )
            .into_response();
    }
    let json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.split(';').next())
        .is_some_and(|s| s.trim().eq_ignore_ascii_case("application/json"));
    if !json {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "expected application/json",
        )
            .into_response();
    }
    if headers
        .get("x-duhem-bundle-version")
        .and_then(|h| h.to_str().ok())
        != Some(BUNDLE_VERSION.to_string().as_str())
    {
        return (StatusCode::BAD_REQUEST, "unsupported bundle version").into_response();
    }
    let Some(hash) = headers
        .get("x-duhem-content-hash")
        .and_then(|h| h.to_str().ok())
        .filter(|s| {
            s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
        .map(str::to_owned)
    else {
        return (StatusCode::BAD_REQUEST, "invalid content hash header").into_response();
    };
    // Use a length-limited body even when Content-Length is absent.
    let body = match Bytes::from_request(request, &()).await {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    match state.store.ingest_bundle(&body, &hash).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e @ IngestError::Invalid(_)) => {
            (StatusCode::BAD_REQUEST, e.to_string()).into_response()
        }
        Err(e @ (IngestError::Conflict | IngestError::MissingParent)) => {
            (StatusCode::CONFLICT, e.to_string()).into_response()
        }
        Err(IngestError::Store(error)) => {
            eprintln!("ingest: {error}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
