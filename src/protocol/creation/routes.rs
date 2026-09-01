use std::collections::HashMap;
use std::net::SocketAddr;

use axum::{
    body::Bytes,
    extract::{ConnectInfo, State},
    response::{IntoResponse, Response},
    Extension,
};
use base64::{engine::general_purpose, Engine};
use http::{HeaderMap, Method, StatusCode, Uri};

use crate::{
    data_storage::base::DataStorage,
    errors::RustusResult,
    file_info::FileInfo,
    info_storage::base::InfoStorage,
    metrics::RustusMetrics,
    notifiers::{Hook, RequestInfo},
    protocol::extensions::Extensions,
    utils::headers::{check_header, parse_header, remote_addr},
    State as RustusState,
};

/// Get metadata info from request.
///
/// Metadata is located in Upload-Metadata header.
/// Key and values are separated by spaces and
/// pairs are delimited with commas.
///
/// E.G.
/// `Upload-Metadata: Video bWVtZXM=,Category bWVtZXM=`
///
/// All values are encoded as base64 strings.
fn get_metadata(headers: &HeaderMap) -> Option<HashMap<String, String>> {
    headers
        .get("Upload-Metadata")
        .and_then(|her| her.to_str().ok())
        .map(String::from)
        .map(|header_string| {
            let mut meta_map = HashMap::new();
            for meta_pair in header_string.split(',') {
                let mut split = meta_pair.trim().split(' ');
                let key = split.next();
                let b64val = split.next();
                if key.is_none() || b64val.is_none() {
                    continue;
                }
                let value = general_purpose::STANDARD
                    .decode(b64val.unwrap())
                    .ok()
                    .and_then(|value| String::from_utf8(value).ok());
                if let Some(res) = value {
                    meta_map.insert(String::from(key.unwrap()), res);
                }
            }
            meta_map
        })
}

fn get_upload_parts(headers: &HeaderMap) -> Vec<String> {
    let concat_header = headers.get("Upload-Concat").unwrap();
    let header_str = concat_header.to_str().unwrap();
    let urls = header_str.strip_prefix("final;").unwrap();

    urls.split(' ')
        .filter_map(|val: &str| val.trim().split('/').last().map(String::from))
        .filter(|val| val.trim() != "")
        .collect()
}

/// Create file.
///
/// This method allows you to create file to start uploading.
///
/// This method supports defer-length if
/// you don't know actual file length and
/// you can upload first bytes if creation-with-upload
/// extension is enabled.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub async fn create_file(
    State(state): State<RustusState>,
    Extension(metrics): Extension<RustusMetrics>,
    method: Method,
    uri: Uri,
    conn: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> RustusResult<Response> {
    let conn = conn.map(|Extension(ConnectInfo(addr))| addr);
    // Getting Upload-Length header value as usize.
    let length = parse_header(&headers, "Upload-Length");

    // With this option enabled,
    // we have to check whether length is a non-zero number.
    if !state.config.allow_empty && length == Some(0) {
        return Ok((
            StatusCode::BAD_REQUEST,
            "Upload-Length should be greater than zero",
        )
            .into_response());
    }

    // Checking Upload-Defer-Length header.
    let defer_size = check_header(&headers, "Upload-Defer-Length", |val| val == "1");

    // Indicator that creation-defer-length is enabled.
    let defer_ext = state
        .config
        .tus_extensions
        .contains(&Extensions::CreationDeferLength);

    let is_final = check_header(&headers, "Upload-Concat", |val| val.starts_with("final;"));

    let concat_ext = state
        .config
        .tus_extensions
        .contains(&Extensions::Concatenation);

    // Check that Upload-Length header is provided.
    // Otherwise checking that defer-size feature is enabled
    // and header provided.
    if length.is_none() && !((defer_ext && defer_size) || (concat_ext && is_final)) {
        return Ok((StatusCode::BAD_REQUEST, "Upload-Length header is required").into_response());
    }

    if state.config.max_file_size.is_some() && state.config.max_file_size < length {
        return Ok((
            StatusCode::BAD_REQUEST,
            format!(
                "Upload-Length should be less than or equal to {}",
                state.config.max_file_size.unwrap()
            ),
        )
            .into_response());
    }

    let meta = get_metadata(&headers);

    let file_id = uuid::Uuid::new_v4().to_string();
    let mut file_info = FileInfo::new(
        file_id.as_str(),
        length,
        None,
        state.data_storage.get_name().to_string(),
        meta,
    );

    let is_partial = check_header(&headers, "Upload-Concat", |val| val == "partial");

    if concat_ext {
        if is_final {
            file_info.is_final = true;
            let upload_parts = get_upload_parts(&headers);
            if upload_parts.is_empty() {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    "Upload-Concat header has no parts to create final upload.",
                )
                    .into_response());
            }
            file_info.parts = Some(upload_parts);
            file_info.deferred_size = false;
        }
        if is_partial {
            file_info.is_partial = true;
        }
    }

    if state.config.hook_is_active(Hook::PreCreate) {
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
        state
            .notification_manager
            .send_message(message, Hook::PreCreate, &cloned_info, &headers)
            .await?;
    }

    // Create file and get the it's path.
    file_info.path = Some(state.data_storage.create_file(&mut file_info).await?);

    // Incrementing number of active uploads

    metrics.active_uploads.inc();
    metrics.started_uploads.inc();

    if let Some(length) = file_info.length {
        #[allow(clippy::cast_precision_loss)]
        metrics.upload_sizes.observe(length as f64);
    }

    if file_info.is_final {
        let mut final_size = 0;
        let mut parts_info = Vec::new();
        for part_id in file_info.clone().parts.unwrap() {
            let part = state.info_storage.get_info(part_id.as_str()).await?;
            if part.length != Some(part.offset) {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    format!("{} upload is not complete.", part.id),
                )
                    .into_response());
            }
            if !part.is_partial {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    format!("{} upload is not partial.", part.id),
                )
                    .into_response());
            }
            final_size += &part.length.unwrap();
            parts_info.push(part.clone());
        }
        state
            .data_storage
            .concat_files(&file_info, parts_info.clone())
            .await?;
        file_info.offset = final_size;
        file_info.length = Some(final_size);
        if state.config.remove_parts {
            for part in parts_info {
                state.data_storage.remove_file(&part).await?;
                state.info_storage.remove_info(part.id.as_str()).await?;
            }
        }
    }

    // Checking if creation-with-upload extension is enabled.
    let with_upload = state
        .config
        .tus_extensions
        .contains(&Extensions::CreationWithUpload);
    if with_upload && !bytes.is_empty() && !(concat_ext && is_final) {
        let octet_stream = |val: &str| val == "application/offset+octet-stream";
        if check_header(&headers, "Content-Type", octet_stream) {
            // Writing first bytes.
            let chunk_len = bytes.len();
            // Appending bytes to file.
            state.data_storage.add_bytes(&mut file_info, bytes).await?;
            // Updating offset.
            file_info.offset += chunk_len;
        }
    }

    state.info_storage.set_info(&file_info, true).await?;

    // It's more intuitive to send post-finish
    // hook, when final upload is created.
    // https://github.com/s3rius/rustus/issues/77
    let post_hook = if file_info.is_final || Some(file_info.offset) == file_info.length {
        Hook::PostFinish
    } else {
        Hook::PostCreate
    };

    if state.config.hook_is_active(post_hook) {
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
        // Adding send_message task to tokio reactor.
        // Thin function would be executed in background.
        let cloned_info = file_info.clone();
        let cloned_state = state.clone();
        let cloned_headers = headers.clone();
        tokio::spawn(async move {
            cloned_state
                .notification_manager
                .send_message(message, post_hook, &cloned_info, &cloned_headers)
                .await
        });
    }

    // Create upload URL for this file.
    let upload_url = format!("/{}/{}", state.config.base_url(), file_info.id);

    Ok((
        StatusCode::CREATED,
        [
            ("Location", upload_url),
            ("Upload-Offset", file_info.offset.to_string()),
        ],
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use crate::{info_storage::base::InfoStorage, server::test::get_service, State};
    use axum::body::Body;
    use base64::{engine::general_purpose, Engine};
    use http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn success() {
        let state = State::test_new().await;
        let rustus = get_service(state.clone());
        let request = Request::builder()
            .method("POST")
            .uri(state.config.test_url())
            .header("Upload-Length", 100)
            .body(Body::empty())
            .unwrap();
        let resp = rustus.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        // Getting file from location header.
        let item_id = resp
            .headers()
            .get("Location")
            .unwrap()
            .to_str()
            .unwrap()
            .split('/')
            .last()
            .unwrap();
        let file_info = state.info_storage.get_info(item_id).await.unwrap();
        assert_eq!(file_info.length, Some(100));
        assert_eq!(file_info.offset, 0);
    }

    #[tokio::test]
    async fn wrong_length() {
        let state = State::test_new().await;
        let rustus = get_service(state.clone());
        let request = Request::builder()
            .method("POST")
            .uri(state.config.test_url())
            .header("Upload-Length", 0)
            .body(Body::empty())
            .unwrap();
        let resp = rustus.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn allow_empty() {
        let mut state = State::test_new().await;
        state.config.allow_empty = true;
        let rustus = get_service(state.clone());
        let request = Request::builder()
            .method("POST")
            .uri(state.config.test_url())
            .header("Upload-Length", 0)
            .body(Body::empty())
            .unwrap();
        let resp = rustus.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn success_with_bytes() {
        let state = State::test_new().await;
        let rustus = get_service(state.clone());
        let test_data = "memes";
        let request = Request::builder()
            .method("POST")
            .uri(state.config.test_url())
            .header("Upload-Length", 100)
            .header("Content-Type", "application/offset+octet-stream")
            .body(Body::from(test_data))
            .unwrap();
        let resp = rustus.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        // Getting file from location header.
        let item_id = resp
            .headers()
            .get("Location")
            .unwrap()
            .to_str()
            .unwrap()
            .split('/')
            .last()
            .unwrap();
        let file_info = state.info_storage.get_info(item_id).await.unwrap();
        assert_eq!(file_info.length, Some(100));
        assert_eq!(file_info.offset, test_data.len());
    }

    #[tokio::test]
    async fn with_bytes_wrong_content_type() {
        let state = State::test_new().await;
        let rustus = get_service(state.clone());
        let test_data = "memes";
        let request = Request::builder()
            .method("POST")
            .uri(state.config.test_url())
            .header("Upload-Length", 100)
            .header("Content-Type", "random")
            .body(Body::from(test_data))
            .unwrap();
        let resp = rustus.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        // Getting file from location header.
        let item_id = resp
            .headers()
            .get("Location")
            .unwrap()
            .to_str()
            .unwrap()
            .split('/')
            .last()
            .unwrap();
        let file_info = state.info_storage.get_info(item_id).await.unwrap();
        assert_eq!(file_info.length, Some(100));
        assert_eq!(file_info.offset, 0);
    }

    #[tokio::test]
    async fn success_defer_size() {
        let state = State::test_new().await;
        let rustus = get_service(state.clone());
        let request = Request::builder()
            .method("POST")
            .uri(state.config.test_url())
            .header("Upload-Defer-Length", "1")
            .body(Body::empty())
            .unwrap();
        let resp = rustus.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        // Getting file from location header.
        let item_id = resp
            .headers()
            .get("Location")
            .unwrap()
            .to_str()
            .unwrap()
            .split('/')
            .last()
            .unwrap();
        let file_info = state.info_storage.get_info(item_id).await.unwrap();
        assert_eq!(file_info.length, None);
        assert!(file_info.deferred_size);
    }

    #[tokio::test]
    async fn success_partial_upload() {
        let state = State::test_new().await;
        let rustus = get_service(state.clone());
        let request = Request::builder()
            .method("POST")
            .uri(state.config.test_url())
            .header("Upload-Length", 100)
            .header("Upload-Concat", "partial")
            .body(Body::empty())
            .unwrap();
        let resp = rustus.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        // Getting file from location header.
        let item_id = resp
            .headers()
            .get("Location")
            .unwrap()
            .to_str()
            .unwrap()
            .split('/')
            .last()
            .unwrap();
        let file_info = state.info_storage.get_info(item_id).await.unwrap();
        assert_eq!(file_info.length, Some(100));
        assert!(file_info.is_partial);
        assert!(!file_info.is_final);
    }

    #[tokio::test]
    async fn success_final_upload() {
        let state = State::test_new().await;
        let rustus = get_service(state.clone());
        let mut part1 = state.create_test_file().await;
        let mut part2 = state.create_test_file().await;
        part1.is_partial = true;
        part1.length = Some(100);
        part1.offset = 100;

        part2.is_partial = true;
        part2.length = Some(100);
        part2.offset = 100;

        state.info_storage.set_info(&part1, false).await.unwrap();
        state.info_storage.set_info(&part2, false).await.unwrap();

        let request = Request::builder()
            .method("POST")
            .uri(state.config.test_url())
            .header("Upload-Length", 100)
            .header(
                "Upload-Concat",
                format!("final;/files/{} /files/{}", part1.id, part2.id),
            )
            .body(Body::empty())
            .unwrap();
        let resp = rustus.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        // Getting file from location header.
        let item_id = resp
            .headers()
            .get("Location")
            .unwrap()
            .to_str()
            .unwrap()
            .split('/')
            .last()
            .unwrap();
        let file_info = state.info_storage.get_info(item_id).await.unwrap();
        assert_eq!(file_info.length, Some(200));
        assert!(file_info.is_final);
    }

    #[tokio::test]
    async fn invalid_final_upload_no_parts() {
        let state = State::test_new().await;
        let rustus = get_service(state.clone());

        let request = Request::builder()
            .method("POST")
            .uri(state.config.test_url())
            .header("Upload-Length", 100)
            .header("Upload-Concat", "final;")
            .body(Body::empty())
            .unwrap();
        let resp = rustus.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn success_with_metadata() {
        let state = State::test_new().await;
        let rustus = get_service(state.clone());
        let request = Request::builder()
            .method("POST")
            .uri(state.config.test_url())
            .header("Upload-Length", 100)
            .header(
                "Upload-Metadata",
                format!(
                    "test {}, pest {}",
                    general_purpose::STANDARD.encode("data1"),
                    general_purpose::STANDARD.encode("data2")
                ),
            )
            .body(Body::empty())
            .unwrap();
        let resp = rustus.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        // Getting file from location header.
        let item_id = resp
            .headers()
            .get("Location")
            .unwrap()
            .to_str()
            .unwrap()
            .split('/')
            .last()
            .unwrap();
        let file_info = state.info_storage.get_info(item_id).await.unwrap();
        assert_eq!(file_info.length, Some(100));
        assert_eq!(file_info.metadata.get("test").unwrap(), "data1");
        assert_eq!(file_info.metadata.get("pest").unwrap(), "data2");
        assert_eq!(file_info.offset, 0);
    }

    #[tokio::test]
    async fn success_with_metadata_wrong_encoding() {
        let state = State::test_new().await;
        let rustus = get_service(state.clone());
        let request = Request::builder()
            .method("POST")
            .uri(state.config.test_url())
            .header("Upload-Length", 100)
            .header(
                "Upload-Metadata",
                format!(
                    "test data1, pest {}",
                    general_purpose::STANDARD.encode("data")
                ),
            )
            .body(Body::empty())
            .unwrap();
        let resp = rustus.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        // Getting file from location header.
        let item_id = resp
            .headers()
            .get("Location")
            .unwrap()
            .to_str()
            .unwrap()
            .split('/')
            .last()
            .unwrap();
        let file_info = state.info_storage.get_info(item_id).await.unwrap();
        assert_eq!(file_info.length, Some(100));
        assert!(!file_info.metadata.contains_key("test"));
        assert_eq!(file_info.metadata.get("pest").unwrap(), "data");
        assert_eq!(file_info.offset, 0);
    }

    #[tokio::test]
    async fn no_length_header() {
        let state = State::test_new().await;
        let rustus = get_service(state.clone());
        let request = Request::builder()
            .method("POST")
            .uri(state.config.test_url())
            .body(Body::empty())
            .unwrap();
        let resp = rustus.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn max_file_size_exceeded() {
        let mut state = State::test_new().await;
        state.config.max_file_size = Some(1000);
        let rustus = get_service(state.clone());
        let request = Request::builder()
            .method("POST")
            .uri(state.config.test_url())
            .header("Upload-Length", 1001)
            .body(Body::empty())
            .unwrap();
        let resp = rustus.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
