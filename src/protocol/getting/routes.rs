use axum::{
    extract::{Path, State},
    response::Response,
};
use http::HeaderMap;

use crate::{
    data_storage::base::DataStorage, errors::RustusError, info_storage::base::InfoStorage,
    RustusResult, State as RustusState,
};

/// Retrieve actual file.
///
/// This method allows you to download files directly from storage.
pub async fn get_file(
    State(state): State<RustusState>,
    Path(file_id): Path<String>,
    headers: HeaderMap,
) -> RustusResult<Response> {
    let file_info = state.info_storage.get_info(file_id.as_str()).await?;
    if file_info.storage != state.data_storage.get_name() {
        return Err(RustusError::FileNotFound);
    }
    state.data_storage.get_contents(&file_info, &headers).await
}

#[cfg(test)]
mod test {
    use crate::{
        data_storage::base::DataStorage, info_storage::base::InfoStorage,
        server::test::get_service, State,
    };
    use axum::body::Body;
    use bytes::Bytes;
    use http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn success() {
        let state = State::test_new().await;
        let rustus = get_service(state.clone());
        let mut file_info = state.create_test_file().await;
        state
            .data_storage
            .add_bytes(&mut file_info, Bytes::from("testing"))
            .await
            .unwrap();
        let request = Request::builder()
            .method("GET")
            .uri(state.config.file_url(file_info.id.as_str()))
            .body(Body::empty())
            .unwrap();
        let resp = rustus.oneshot(request).await.unwrap();
        assert!(resp.status().is_success());
    }

    #[tokio::test]
    async fn unknown_file_id() {
        let state = State::test_new().await;
        let rustus = get_service(state.clone());
        let request = Request::builder()
            .method("GET")
            .uri(state.config.file_url("random_str"))
            .body(Body::empty())
            .unwrap();
        let resp = rustus.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn unknown_storage() {
        let state = State::test_new().await;
        let rustus = get_service(state.clone());
        let mut file_info = state.create_test_file().await;
        file_info.storage = "unknown_storage".into();
        state
            .info_storage
            .set_info(&file_info, false)
            .await
            .unwrap();
        let request = Request::builder()
            .method("GET")
            .uri(state.config.file_url(file_info.id.as_str()))
            .body(Body::empty())
            .unwrap();
        let resp = rustus.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
