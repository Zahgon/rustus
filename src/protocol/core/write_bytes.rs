use std::net::SocketAddr;

use axum::{
    body::Bytes,
    extract::{ConnectInfo, Path, State},
    response::{IntoResponse, Response},
};
use http::{HeaderMap, Method, StatusCode, Uri};

use crate::{
    data_storage::base::DataStorage,
    errors::RustusError,
    info_storage::base::InfoStorage,
    metrics::RustusMetrics,
    notifiers::{Hook, RequestInfo},
    protocol::extensions::Extensions,
    utils::{
        hashes::verify_chunk_checksum,
        headers::{check_header, parse_header, remote_addr},
    },
    RustusResult, State as RustusState,
};

use axum::Extension;

#[allow(clippy::too_many_arguments)]
pub async fn write_bytes(
    State(state): State<RustusState>,
    Extension(metrics): Extension<RustusMetrics>,
    Path(file_id): Path<String>,
    method: Method,
    uri: Uri,
    conn: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> RustusResult<Response> {
    let conn = conn.map(|Extension(ConnectInfo(addr))| addr);
    // Checking if request has required headers.
    let check_content_type = |val: &str| val == "application/offset+octet-stream";
    if !check_header(&headers, "Content-Type", check_content_type) {
        return Ok((StatusCode::UNSUPPORTED_MEDIA_TYPE, "Unknown content-type.").into_response());
    }
    // Getting current offset.
    let offset: Option<usize> = parse_header(&headers, "Upload-Offset");

    if offset.is_none() {
        return Ok((StatusCode::UNSUPPORTED_MEDIA_TYPE, "No offset provided.").into_response());
    }

    if state.config.tus_extensions.contains(&Extensions::Checksum) {
        if let Some(header) = headers.get("Upload-Checksum").cloned() {
            let cloned_bytes = bytes.clone();
            if !tokio::task::spawn_blocking(move || {
                verify_chunk_checksum(&header, cloned_bytes.as_ref())
            })
            .await??
            {
                return Err(RustusError::WrongChecksum);
            }
        }
    }

    // New upload length.
    // Parses header `Upload-Length` only if the creation-defer-length extension is enabled.
    let updated_len = if state
        .config
        .tus_extensions
        .contains(&Extensions::CreationDeferLength)
    {
        parse_header(&headers, "Upload-Length")
    } else {
        None
    };

    // Getting file info.
    let mut file_info = state.info_storage.get_info(&file_id).await?;

    // According to TUS protocol you can't update final uploads.
    if file_info.is_final {
        return Ok(StatusCode::FORBIDDEN.into_response());
    }

    // Checking if file was stored in the same storage.
    if file_info.storage != state.data_storage.get_name() {
        return Err(RustusError::FileNotFound);
    }
    // Checking if offset from request is the same as the real offset.
    if offset.unwrap() != file_info.offset {
        return Ok(StatusCode::CONFLICT.into_response());
    }

    // If someone want to update file length.
    // This required by Upload-Defer-Length extension.
    if let Some(new_len) = updated_len {
        // Whoop, someone gave us total file length
        // less that he had already uploaded.
        if new_len < file_info.offset {
            return Err(RustusError::WrongOffset);
        }
        // We already know the exact size of a file.
        // Someone want to update it.
        // Anyway, it's not allowed, heh.
        if file_info.length.is_some() {
            return Err(RustusError::SizeAlreadyKnown);
        }

        // All checks are ok. Now our file will have exact size.
        file_info.deferred_size = false;
        file_info.length = Some(new_len);
    }

    // Checking if the size of the upload is already equals
    // to calculated offset. It means that all bytes were already written.
    if Some(file_info.offset) == file_info.length {
        return Err(RustusError::FrozenFile);
    }
    let chunk_len = bytes.len();
    // Appending bytes to file.
    state.data_storage.add_bytes(&mut file_info, bytes).await?;
    // bytes.clear()
    // Updating offset.
    file_info.offset += chunk_len;
    // Saving info to info storage.
    state.info_storage.set_info(&file_info, false).await?;

    let hook = if file_info.length == Some(file_info.offset) {
        Hook::PostFinish
    } else {
        Hook::PostReceive
    };
    if state.config.hook_is_active(hook) {
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
        let headers = headers.clone();
        let cloned_info = file_info.clone();
        let cloned_state = state.clone();
        tokio::spawn(async move {
            cloned_state
                .notification_manager
                .send_message(message, hook, &cloned_info, &headers)
                .await
        });
    }

    if hook == Hook::PostFinish {
        metrics.active_uploads.dec();
        metrics.finished_uploads.inc();
    }

    Ok((
        StatusCode::NO_CONTENT,
        [
            ("Upload-Offset", file_info.offset.to_string()),
            ("Cache-Control", "no-cache".to_string()),
        ],
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use crate::{info_storage::base::InfoStorage, server::test::get_service, State};
    use axum::body::Body;
    use http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    /// Success test for writing bytes.
    ///
    /// This test creates file and writes bytes to it.
    async fn success() {
        let state = State::test_new().await;
        let rustus = get_service(state.clone());
        let mut file = state.create_test_file().await;
        file.length = Some(100);
        file.offset = 0;
        state.info_storage.set_info(&file, false).await.unwrap();
        let test_data = "memes";
        let request = Request::builder()
            .method("PATCH")
            .uri(state.config.file_url(file.id.as_str()).as_str())
            .header("Content-Type", "application/offset+octet-stream")
            .header("Upload-Checksum", "md5 xIwpFX4rNYzBRAJ/Pi2MtA==")
            .header("Upload-Offset", file.offset.to_string())
            .body(Body::from(test_data))
            .unwrap();
        let resp = rustus.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            resp.headers()
                .get("Upload-Offset")
                .unwrap()
                .to_str()
                .unwrap(),
            test_data.len().to_string().as_str()
        );
        let new_info = state
            .info_storage
            .get_info(file.id.clone().as_str())
            .await
            .unwrap();
        assert_eq!(new_info.offset, test_data.len());
    }

    #[tokio::test]
    /// Testing defer-length extension.
    ///
    /// During this test we'll try to update
    /// file's length while writing bytes to it.
    async fn success_update_file_length() {
        let state = State::test_new().await;
        let rustus = get_service(state.clone());
        let mut file = state.create_test_file().await;
        file.length = None;
        file.deferred_size = true;
        file.offset = 0;
        state.info_storage.set_info(&file, false).await.unwrap();
        let test_data = "memes";
        let request = Request::builder()
            .method("PATCH")
            .uri(state.config.file_url(file.id.as_str()).as_str())
            .header("Content-Type", "application/offset+octet-stream")
            .header("Upload-Offset", file.offset.to_string())
            .header("Upload-Length", "20")
            .body(Body::from(test_data))
            .unwrap();
        let resp = rustus.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            resp.headers()
                .get("Upload-Offset")
                .unwrap()
                .to_str()
                .unwrap(),
            test_data.len().to_string().as_str()
        );
        let new_info = state
            .info_storage
            .get_info(file.id.clone().as_str())
            .await
            .unwrap();
        assert_eq!(new_info.offset, test_data.len());
        assert!(!new_info.deferred_size);
        assert_eq!(new_info.length, Some(20));
    }

    #[tokio::test]
    /// Tests that if new file length
    /// is less than current offset, error is thrown.
    async fn new_file_length_lt_offset() {
        let state = State::test_new().await;
        let rustus = get_service(state.clone());
        let mut file = state.create_test_file().await;
        file.length = None;
        file.deferred_size = true;
        file.offset = 30;
        state.info_storage.set_info(&file, false).await.unwrap();
        let test_data = "memes";
        let request = Request::builder()
            .method("PATCH")
            .uri(state.config.file_url(file.id.as_str()).as_str())
            .header("Content-Type", "application/offset+octet-stream")
            .header("Upload-Offset", file.offset.to_string())
            .header("Upload-Length", "20")
            .body(Body::from(test_data))
            .unwrap();
        let resp = rustus.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    /// Tests if user tries to update
    /// file length with known length,
    /// error is thrown.
    async fn new_file_length_size_already_known() {
        let state = State::test_new().await;
        let rustus = get_service(state.clone());
        let mut file = state.create_test_file().await;
        file.length = Some(100);
        file.deferred_size = false;
        file.offset = 0;
        state.info_storage.set_info(&file, false).await.unwrap();
        let test_data = "memes";
        let request = Request::builder()
            .method("PATCH")
            .uri(state.config.file_url(file.id.as_str()).as_str())
            .header("Content-Type", "application/offset+octet-stream")
            .header("Upload-Offset", file.offset.to_string())
            .header("Upload-Length", "120")
            .body(Body::from(test_data))
            .unwrap();
        let resp = rustus.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    /// Checks that if Content-Type header missing,
    /// wrong status code is returned.
    async fn no_content_header() {
        let state = State::test_new().await;
        let rustus = get_service(state.clone());
        let mut file = state.create_test_file().await;
        file.length = Some(100);
        file.offset = 0;
        state.info_storage.set_info(&file, false).await.unwrap();
        let request = Request::builder()
            .method("PATCH")
            .uri(state.config.file_url(file.id.as_str()).as_str())
            .header("Upload-Offset", "0")
            .body(Body::from("memes"))
            .unwrap();
        let resp = rustus.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[tokio::test]
    /// Tests that method will return error if no offset header specified.
    async fn no_offset_header() {
        let state = State::test_new().await;
        let rustus = get_service(state.clone());
        let mut file = state.create_test_file().await;
        file.length = Some(100);
        file.offset = 0;
        state.info_storage.set_info(&file, false).await.unwrap();
        let request = Request::builder()
            .method("PATCH")
            .uri(state.config.file_url(file.id.as_str()).as_str())
            .header("Content-Type", "application/offset+octet-stream")
            .body(Body::from("memes"))
            .unwrap();
        let resp = rustus.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[tokio::test]
    /// Tests that method will return error if wrong offset is passed.
    async fn wrong_offset_header() {
        let state = State::test_new().await;
        let rustus = get_service(state.clone());
        let mut file = state.create_test_file().await;
        file.length = Some(100);
        file.offset = 0;
        state.info_storage.set_info(&file, false).await.unwrap();
        let request = Request::builder()
            .method("PATCH")
            .uri(state.config.file_url(file.id.as_str()).as_str())
            .header("Upload-Offset", "1")
            .header("Content-Type", "application/offset+octet-stream")
            .body(Body::from("memes"))
            .unwrap();
        let resp = rustus.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    /// Tests that method would return error if file was already uploaded.
    async fn final_upload() {
        let state = State::test_new().await;
        let rustus = get_service(state.clone());
        let mut file = state.create_test_file().await;
        file.is_final = true;
        state.info_storage.set_info(&file, false).await.unwrap();
        let request = Request::builder()
            .method("PATCH")
            .uri(state.config.file_url(file.id.as_str()).as_str())
            .header("Upload-Offset", file.offset.to_string())
            .header("Content-Type", "application/offset+octet-stream")
            .body(Body::from("memes"))
            .unwrap();
        let resp = rustus.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    /// Tests that method would return 404 if file was saved in other storage.
    async fn wrong_storage() {
        let state = State::test_new().await;
        let rustus = get_service(state.clone());
        let mut file = state.create_test_file().await;
        file.storage = "unknown".into();
        state.info_storage.set_info(&file, false).await.unwrap();
        let request = Request::builder()
            .method("PATCH")
            .uri(state.config.file_url(file.id.as_str()).as_str())
            .header("Upload-Offset", file.offset.to_string())
            .header("Content-Type", "application/offset+octet-stream")
            .body(Body::from("memes"))
            .unwrap();
        let resp = rustus.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    /// Tests that method won't allow you to update
    /// file if it's offset already equal to length.
    async fn frozen_file() {
        let state = State::test_new().await;
        let rustus = get_service(state.clone());
        let mut file = state.create_test_file().await;
        file.offset = 10;
        file.length = Some(10);
        state.info_storage.set_info(&file, false).await.unwrap();
        let request = Request::builder()
            .method("PATCH")
            .uri(state.config.file_url(file.id.as_str()).as_str())
            .header("Upload-Offset", file.offset.to_string())
            .header("Content-Type", "application/offset+octet-stream")
            .body(Body::from("memes"))
            .unwrap();
        let resp = rustus.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    /// Tests that method will return 404 if
    /// unknown file_id is passed.
    async fn unknown_file_id() {
        let state = State::test_new().await;
        let rustus = get_service(state.clone());
        let request = Request::builder()
            .method("PATCH")
            .uri(state.config.file_url("unknown").as_str())
            .header("Upload-Offset", "0")
            .header("Content-Type", "application/offset+octet-stream")
            .body(Body::from("memes"))
            .unwrap();
        let resp = rustus.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    /// Tests checksum validation.
    async fn wrong_checksum() {
        let state = State::test_new().await;
        let rustus = get_service(state.clone());
        let mut file = state.create_test_file().await;
        file.offset = 0;
        file.length = Some(10);
        state.info_storage.set_info(&file, false).await.unwrap();
        let request = Request::builder()
            .method("PATCH")
            .uri(state.config.file_url(file.id.as_str()).as_str())
            .header("Upload-Offset", "0")
            .header("Upload-Checksum", "md5 K9opmNmw7hl9oUKgRH9nJQ==")
            .header("Content-Type", "application/offset+octet-stream")
            .body(Body::from("memes"))
            .unwrap();
        let resp = rustus.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::EXPECTATION_FAILED);
    }
}
