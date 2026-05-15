use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};

use crate::blob::{BlobKey, BlobStore, PutError};

pub async fn put_blob(
    State(store): State<BlobStore>,
    Path((group_id, sha256_hex)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    if !is_valid_sha256_hex(&sha256_hex) {
        return (StatusCode::BAD_REQUEST, "invalid sha256").into_response();
    }
    match store.put(
        BlobKey {
            group_id,
            sha256_hex,
        },
        body,
    ) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(PutError::TooLarge { max }) => (
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("blob exceeds {max} bytes"),
        )
            .into_response(),
        Err(PutError::HashMismatch) => {
            (StatusCode::BAD_REQUEST, "body sha256 mismatch").into_response()
        }
    }
}

pub async fn get_blob(
    State(store): State<BlobStore>,
    Path((group_id, sha256_hex)): Path<(String, String)>,
) -> Response {
    if !is_valid_sha256_hex(&sha256_hex) {
        return (StatusCode::BAD_REQUEST, "invalid sha256").into_response();
    }
    match store.get(&BlobKey {
        group_id,
        sha256_hex,
    }) {
        Some(bytes) => {
            ([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn is_valid_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}
