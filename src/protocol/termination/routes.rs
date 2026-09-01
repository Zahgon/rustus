use std::net::SocketAddr;

use axum::{
    extract::{ConnectInfo, Path, State},
    response::{IntoResponse, Response},
    Extension,
};
use http::{HeaderMap, Method, StatusCode, Uri};

use crate::{
    data_storage::base::DataStorage,
    errors::{RustusError, RustusResult},
    info_storage::base::InfoStorage,
    metrics::RustusMetrics,
    notifiers::{Hook, RequestInfo},
    utils::headers::remote_addr,
    State as RustusState,
};

/// Terminate uploading.
///
/// This method will remove all data by id.
/// It removes info and actual data.
#[allow(clippy::too_many_arguments)]
pub async fn terminate(
    State(state): State<RustusState>,
    Extension(metrics): Extension<RustusMetrics>,
    Path(file_id): Path<String>,
    method: Method,
    uri: Uri,
    conn: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
) -> RustusResult<Response> {
    let conn = conn.map(|Extension(ConnectInfo(addr))| addr);
    let file_info = state.info_storage.get_info(file_id.as_str()).await?;
    if file_info.storage != state.data_storage.get_name() {
        return Err(RustusError::FileNotFound);
    }
    if state.config.hook_is_active(Hook::PreTerminate) {
        let request_info = RequestInfo::new(
            uri.to_string(),
            method.to_string(),
            remote_addr(&headers, conn, state.config.notification_opts.behind_proxy),
            headers.clone(),
        );
        let message = state
            .config
            .notification_opts
            .hooks_format
            .format(&request_info, &file_info);
        state
            .notification_manager
            .send_message(message, Hook::PreTerminate, &file_info, &headers)
            .await?;
    }
    state.info_storage.remove_info(file_id.as_str()).await?;
    state.data_storage.remove_file(&file_info).await?;
    metrics.terminated_uploads.inc();
    if state.config.hook_is_active(Hook::PostTerminate) {
        let request_info = RequestInfo::new(
            uri.to_string(),
            method.to_string(),
            remote_addr(&headers, conn, state.config.notification_opts.behind_proxy),
            headers.clone(),
        );
        let message = state
            .config
            .notification_opts
            .hooks_format
            .format(&request_info, &file_info);
        let cloned_info = file_info.clone();
        let cloned_state = state.clone();
        tokio::spawn(async move {
            cloned_state
                .notification_manager
                .send_message(message, Hook::PostTerminate, &cloned_info, &headers)
                .await
        });
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[cfg(test)]
mod tests {
    use crate::{info_storage::base::InfoStorage, server::test::get_service, State};
    use axum::body::Body;
    use http::{Request, StatusCode};
    use std::path::PathBuf;
    use tower::ServiceExt;

    #[tokio::test]
    async fn success() {
        let state = State::test_new().await;
        let rustus = get_service(state.clone());
        let file_info = state.create_test_file().await;
        let request = Request::builder()
            .method("DELETE")
            .uri(state.config.file_url(file_info.id.as_str()))
            .body(Body::empty())
            .unwrap();
        let response = rustus.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(state
            .info_storage
            .get_info(file_info.id.as_str())
            .await
            .is_err());
        assert!(!PathBuf::from(file_info.path.unwrap()).exists());
    }

    #[tokio::test]
    async fn unknown_file_id() {
        let state = State::test_new().await;
        let rustus = get_service(state.clone());
        let request = Request::builder()
            .method("DELETE")
            .uri(state.config.file_url("not_exists"))
            .body(Body::empty())
            .unwrap();
        let result = rustus.oneshot(request).await.unwrap();
        assert_eq!(result.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn wrong_storage() {
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
            .method("DELETE")
            .uri(state.config.file_url(file_info.id.as_str()))
            .body(Body::empty())
            .unwrap();
        let response = rustus.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
