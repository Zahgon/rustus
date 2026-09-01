use http::StatusCode;

/// Default response to all unknown URLs.
/// All protocol urls can be found
/// at `crate::protocol::*`.
#[allow(clippy::unused_async)]
pub async fn not_found() -> StatusCode {
    StatusCode::NOT_FOUND
}

/// Checks that application is accepting connections correctly.
#[allow(clippy::unused_async)]
pub async fn health_check() -> StatusCode {
    StatusCode::OK
}
