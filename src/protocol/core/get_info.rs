use axum::{
    body::Body,
    extract::{Path, State},
    response::Response,
};
use http::StatusCode;

use crate::{
    data_storage::base::DataStorage, errors::RustusError, info_storage::base::InfoStorage,
    RustusResult, State as RustusState,
};

pub async fn get_file_info(
    State(state): State<RustusState>,
    Path(file_id): Path<String>,
) -> RustusResult<Response> {
    // Getting file info from info_storage.
    let file_info = state.info_storage.get_info(&file_id).await?;
    if file_info.storage != state.data_storage.get_name() {
        return Err(RustusError::FileNotFound);
    }
    let mut builder = Response::builder().status(StatusCode::OK);
    if file_info.is_partial {
        builder = builder.header("Upload-Concat", "partial");
    }
    if file_info.is_final && file_info.parts.is_some() {
        let parts = file_info
            .parts
            .clone()
            .unwrap()
            .iter()
            .map(|file| format!("/{}/{}", state.config.base_url(), file.as_str()))
            .collect::<Vec<String>>()
            .join(" ");
        builder = builder.header("Upload-Concat", format!("final; {parts}"));
    }
    builder = builder.header("Upload-Offset", file_info.offset.to_string());
    // Upload length is known.
    if let Some(upload_len) = file_info.length {
        builder = builder
            .header("Content-Length", file_info.offset.to_string())
            .header("Upload-Length", upload_len.to_string());
    } else {
        builder = builder.header("Upload-Defer-Length", "1");
    }
    if let Some(meta) = file_info.get_metadata_string() {
        builder = builder.header("Upload-Metadata", meta);
    }
    builder = builder.header(
        "Upload-Created",
        file_info.created_at.timestamp().to_string(),
    );
    builder = builder.header("Cache-Control", "no-cache");
    // Header to prevent the client and/or proxies from caching the response.
    builder = builder.header("Cache-Control", "no-store");
    builder
        .body(Body::empty())
        .map_err(|_| RustusError::Unknown)
}

#[cfg(test)]
mod tests {
    use crate::{info_storage::base::InfoStorage, server::test::get_service, State};
    use axum::body::Body;
    use http::{Method, Request, StatusCode};
    use tower::ServiceExt;

    use base64::{engine::general_purpose, Engine};

    #[tokio::test]
    async fn success() {
        let state = State::test_new().await;
        let rustus = get_service(state.clone());
        let mut file_info = state.create_test_file().await;
        file_info.offset = 100;
        file_info.length = Some(100);
        state
            .info_storage
            .set_info(&file_info, false)
            .await
            .unwrap();
        let request = Request::builder()
            .method(Method::HEAD)
            .uri(state.config.file_url(file_info.id.as_str()))
            .body(Body::empty())
            .unwrap();
        let response = rustus.oneshot(request).await.unwrap();
        let offset = response
            .headers()
            .get("Upload-Offset")
            .unwrap()
            .to_str()
            .unwrap()
            .parse::<usize>()
            .unwrap();
        assert_eq!(file_info.offset, offset);
    }

    #[tokio::test]
    async fn success_metadata() {
        let state = State::test_new().await;
        let rustus = get_service(state.clone());
        let mut file_info = state.create_test_file().await;
        file_info.offset = 100;
        file_info.length = Some(100);
        file_info.metadata.insert("test".into(), "value".into());
        state
            .info_storage
            .set_info(&file_info, false)
            .await
            .unwrap();
        let request = Request::builder()
            .method(Method::HEAD)
            .uri(state.config.file_url(file_info.id.as_str()))
            .body(Body::empty())
            .unwrap();
        let response = rustus.oneshot(request).await.unwrap();
        let metadata = response
            .headers()
            .get("Upload-Metadata")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(
            String::from(metadata),
            format!("{} {}", "test", general_purpose::STANDARD.encode("value"))
        );
    }

    #[tokio::test]
    async fn success_defer_len() {
        let state = State::test_new().await;
        let rustus = get_service(state.clone());
        let mut file_info = state.create_test_file().await;
        file_info.deferred_size = true;
        file_info.length = None;
        state
            .info_storage
            .set_info(&file_info, false)
            .await
            .unwrap();
        let request = Request::builder()
            .method(Method::HEAD)
            .uri(state.config.file_url(file_info.id.as_str()))
            .body(Body::empty())
            .unwrap();
        let response = rustus.oneshot(request).await.unwrap();
        assert_eq!(
            response
                .headers()
                .get("Upload-Defer-Length")
                .unwrap()
                .to_str()
                .unwrap(),
            "1"
        );
    }

    #[tokio::test]
    async fn test_get_file_info_partial() {
        let state = State::test_new().await;
        let rustus = get_service(state.clone());
        let mut file_info = state.create_test_file().await;
        file_info.is_partial = true;
        state
            .info_storage
            .set_info(&file_info, false)
            .await
            .unwrap();
        let request = Request::builder()
            .method(Method::HEAD)
            .uri(state.config.file_url(file_info.id.as_str()))
            .body(Body::empty())
            .unwrap();
        let response = rustus.oneshot(request).await.unwrap();
        assert_eq!(
            response
                .headers()
                .get("Upload-Concat")
                .unwrap()
                .to_str()
                .unwrap(),
            "partial"
        );
    }

    #[tokio::test]
    async fn success_final() {
        let state = State::test_new().await;
        let rustus = get_service(state.clone());
        let mut file_info = state.create_test_file().await;
        file_info.is_partial = false;
        file_info.is_final = true;
        file_info.parts = Some(vec!["test1".into(), "test2".into()]);
        state
            .info_storage
            .set_info(&file_info, false)
            .await
            .unwrap();
        let request = Request::builder()
            .method(Method::HEAD)
            .uri(state.config.file_url(file_info.id.as_str()))
            .body(Body::empty())
            .unwrap();
        let response = rustus.oneshot(request).await.unwrap();
        assert_eq!(
            response
                .headers()
                .get("Upload-Concat")
                .unwrap()
                .to_str()
                .unwrap(),
            format!(
                "final; {} {}",
                state.config.file_url("test1").strip_suffix('/').unwrap(),
                state.config.file_url("test2").strip_suffix('/').unwrap()
            )
            .as_str()
        );
    }

    #[tokio::test]
    async fn no_file() {
        let state = State::test_new().await;
        let rustus = get_service(state.clone());
        let request = Request::builder()
            .method(Method::HEAD)
            .uri(state.config.file_url("unknknown"))
            .body(Body::empty())
            .unwrap();
        let response = rustus.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_get_file_info_wrong_storage() {
        let state = State::test_new().await;
        let rustus = get_service(state.clone());
        let mut file_info = state.create_test_file().await;
        file_info.storage = String::from("unknown");
        state
            .info_storage
            .set_info(&file_info, false)
            .await
            .unwrap();
        let request = Request::builder()
            .method(Method::HEAD)
            .uri(state.config.file_url(file_info.id.as_str()))
            .body(Body::empty())
            .unwrap();
        let response = rustus.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
