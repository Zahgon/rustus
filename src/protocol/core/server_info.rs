use crate::protocol::extensions::Extensions;
use axum::{extract::State, response::IntoResponse};
use http::StatusCode;

use crate::State as RustusState;

#[allow(clippy::needless_pass_by_value)]
#[allow(clippy::unused_async)]
pub async fn server_info(State(state): State<RustusState>) -> impl IntoResponse {
    let ext_str = state
        .config
        .tus_extensions
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<String>>()
        .join(",");
    let mut headers = http::HeaderMap::new();
    if let Ok(value) = http::HeaderValue::from_str(ext_str.as_str()) {
        headers.insert("Tus-Extension", value);
    }
    if state.config.tus_extensions.contains(&Extensions::Checksum) {
        headers.insert(
            "Tus-Checksum-Algorithm",
            http::HeaderValue::from_static("md5,sha1,sha256,sha512"),
        );
    }
    (StatusCode::OK, headers)
}

#[cfg(test)]
mod tests {
    use crate::{protocol::extensions::Extensions, server::test::get_service, State};
    use axum::body::Body;
    use http::{Method, Request};
    use tower::ServiceExt;

    #[tokio::test]
    async fn test_server_info() {
        let mut state = State::test_new().await;
        let rustus = get_service(state.clone());
        state.config.tus_extensions = vec![
            Extensions::Creation,
            Extensions::Concatenation,
            Extensions::Termination,
        ];
        let request = Request::builder()
            .method(Method::OPTIONS)
            .uri(state.config.test_url().as_str())
            .body(Body::empty())
            .unwrap();
        let response = rustus.oneshot(request).await.unwrap();
        let extensions = response
            .headers()
            .get("Tus-Extension")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(extensions.contains(Extensions::Creation.to_string().as_str()));
        assert!(extensions.contains(Extensions::Concatenation.to_string().as_str()));
        assert!(extensions.contains(Extensions::Termination.to_string().as_str()));
    }
}
