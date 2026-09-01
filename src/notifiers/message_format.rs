use crate::{file_info::FileInfo, from_str};
use derive_more::{Display, From};
use http::HeaderMap;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::str::FromStr;
use strum::EnumIter;
use strum::IntoEnumIterator;

/// Framework-agnostic snapshot of the parts of an HTTP request
/// that hook message formats need.
///
/// The web framework extracts these values from the incoming request
/// and hands them to [`Format::format`], so message formatting does
/// not depend on any particular framework's request type.
pub struct RequestInfo {
    pub uri: String,
    pub method: String,
    pub remote_addr: Option<String>,
    pub headers: HeaderMap,
}

impl RequestInfo {
    #[must_use]
    pub fn new(
        uri: String,
        method: String,
        remote_addr: Option<String>,
        headers: HeaderMap,
    ) -> Self {
        Self {
            uri,
            method,
            remote_addr,
            headers,
        }
    }
}

#[derive(Clone, Debug, Eq, Display, From, PartialEq, EnumIter)]
pub enum Format {
    #[display("default")]
    Default,
    #[display("tusd")]
    Tusd,
    #[display("v2")]
    V2,
}

from_str!(Format, "format");

impl Format {
    #[must_use]
    pub fn format(&self, request: &RequestInfo, file_info: &FileInfo) -> String {
        match self {
            Self::Default => default_format(request, file_info),
            Self::Tusd => tusd_format(request, file_info),
            Self::V2 => rustus_format_v2(request, file_info),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct TusdStorageInfo {
    #[serde(rename = "Type")]
    storage_type: String,
    path: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct TusdFileInfo {
    #[serde(rename = "ID")]
    id: String,
    offset: usize,
    size: Option<usize>,
    is_final: bool,
    is_partial: bool,
    partial_uploads: Option<Vec<String>>,
    size_is_deferred: bool,
    meta_data: HashMap<String, String>,
    storage: TusdStorageInfo,
}

impl From<&FileInfo> for TusdFileInfo {
    fn from(file_info: &FileInfo) -> Self {
        let deferred_size = file_info.length.is_none();
        Self {
            id: file_info.id.clone(),
            offset: file_info.offset,
            size: file_info.length,
            size_is_deferred: deferred_size,
            is_final: file_info.is_final,
            is_partial: file_info.is_partial,
            partial_uploads: file_info.parts.clone(),
            meta_data: file_info.metadata.clone(),
            storage: TusdStorageInfo {
                storage_type: file_info.storage.clone(),
                path: file_info.path.clone(),
            },
        }
    }
}

/// Transforms headersmap to `HashMap`.
///
/// Keys of the resulting map are Strings,
/// Values are serde values. It ca be either string values or
/// arrays.
fn headers_to_value_map(headers: &HeaderMap, use_arrays: bool) -> HashMap<String, Value> {
    let mut headers_map = HashMap::new();
    for (name, value) in headers {
        if let Ok(header_val) = value.to_str().map(String::from) {
            if use_arrays {
                headers_map.insert(
                    name.to_string(),
                    Value::Array(vec![Value::String(header_val)]),
                );
            } else {
                headers_map.insert(name.to_string(), Value::String(header_val));
            }
        }
    }
    headers_map
}

/// Default format is specific for Rustus.
///
/// This format is a simple serialized `FileInfo` and some parts of the request.
#[must_use]
pub fn default_format(request: &RequestInfo, file_info: &FileInfo) -> String {
    let value = json!({
        "upload": file_info,
        "request": {
            "URI": request.uri,
            "method": request.method,
            "remote_addr": request.remote_addr,
            "headers": headers_to_value_map(&request.headers, false)
        }
    });
    value.to_string()
}

/// Default format is specific for Rustus V2.
///
/// This format is almost the same as V1, but with some enhancements.
#[must_use]
pub fn rustus_format_v2(request: &RequestInfo, file_info: &FileInfo) -> String {
    let value = json!({
        "upload": file_info,
        "request": {
            "uri": request.uri,
            "method": request.method,
            "remote_addr": request.remote_addr,
            "headers": headers_to_value_map(&request.headers, false)
        }
    });
    value.to_string()
}

/// This format follows TUSD hooks.
///
/// You can read more about tusd hooks
/// [here](https://github.com/tus/tusd/blob/master/docs/hooks.md).
///
/// Generally speaking, it's almost the same as the default format,
/// but some variables are ommited and headers are added to the request.
#[must_use]
pub fn tusd_format(request: &RequestInfo, file_info: &FileInfo) -> String {
    let value = json!({
        "Upload": TusdFileInfo::from(file_info),
        "HTTPRequest": {
            "URI": request.uri,
            "Method": request.method,
            "RemoteAddr": request.remote_addr,
            "Header": headers_to_value_map(&request.headers, true)
        }
    });
    value.to_string()
}
