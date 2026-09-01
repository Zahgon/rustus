use std::{net::SocketAddr, str::FromStr};

use http::{
    header::{HeaderValue, CONTENT_DISPOSITION},
    HeaderMap,
};

/// Parse header's value.
///
/// This function will try to parse
/// header's value to some type T.
///
/// If header is not present or value
/// can't be parsed then it returns None.
pub fn parse_header<T: FromStr>(headers: &HeaderMap, header_name: &str) -> Option<T> {
    headers
        // Get header
        .get(header_name)
        // Parsing it to string.
        .and_then(|value| value.to_str().ok())
        // Parsing to type T.
        .and_then(|val| val.parse::<T>().ok())
}

/// Check that header value satisfies some predicate.
///
/// Passes header as a parameter to expr if header is present.
pub fn check_header(headers: &HeaderMap, header_name: &str, expr: fn(&str) -> bool) -> bool {
    headers
        .get(header_name)
        // Parsing it to string.
        .and_then(|header_val| header_val.to_str().ok())
        // Applying predicate.
        .is_some_and(expr)
}

/// Resolve the remote address of a request.
///
/// When `behind_proxy` is set, the client address is taken from the
/// `Forwarded` / `X-Forwarded-For` headers. Otherwise the peer address
/// from the connection info is used (if available).
pub fn remote_addr(
    headers: &HeaderMap,
    conn: Option<SocketAddr>,
    behind_proxy: bool,
) -> Option<String> {
    if behind_proxy {
        if let Some(forwarded) = headers.get("Forwarded").and_then(|v| v.to_str().ok()) {
            for part in forwarded.split(';') {
                let part = part.trim();
                if let Some(rest) = part.strip_prefix("for=") {
                    let cleaned = rest.trim_matches('"').trim_start_matches('[');
                    if !cleaned.is_empty() {
                        return Some(cleaned.to_string());
                    }
                }
            }
        }
        if let Some(forwarded_for) = headers.get("X-Forwarded-For").and_then(|v| v.to_str().ok()) {
            if let Some(first) = forwarded_for.split(',').next() {
                let trimmed = first.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
        return None;
    }
    conn.map(|addr| addr.to_string())
}

/// This function generates a `Content-Disposition` header value
/// based on file name.
pub fn generate_disposition(filename: &str) -> (http::HeaderName, HeaderValue) {
    let mime_type = mime_guess::from_path(filename).first_or_octet_stream();
    let disposition = match mime_type.type_() {
        mime::IMAGE | mime::TEXT | mime::AUDIO | mime::VIDEO => "inline",
        mime::APPLICATION => match mime_type.subtype() {
            mime::JAVASCRIPT | mime::JSON => "inline",
            name if name == "wasm" => "inline",
            _ => "attachment",
        },
        _ => "attachment",
    };

    let value = format!("{disposition}; filename=\"{filename}\"");
    let header_value =
        HeaderValue::from_str(&value).unwrap_or_else(|_| HeaderValue::from_static("attachment"));
    (CONTENT_DISPOSITION, header_value)
}

#[cfg(test)]
mod tests {
    use super::{check_header, parse_header};
    use http::HeaderMap;

    fn header_map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                http::HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn test_parse_header_unknown_header() {
        let headers = HeaderMap::new();
        let header = parse_header::<String>(&headers, "unknown");
        assert!(header.is_none());
    }

    #[test]
    fn test_parse_header_wrong_type() {
        let headers = header_map(&[("test_header", "test")]);
        let header = parse_header::<i32>(&headers, "test_header");
        assert!(header.is_none());
    }

    #[test]
    fn test_parse_header() {
        let headers = header_map(&[("test_header", "123")]);
        let header = parse_header::<usize>(&headers, "test_header");
        assert_eq!(header.unwrap(), 123);
    }

    #[test]
    fn test_check_header_unknown_header() {
        let headers = HeaderMap::new();
        let check = check_header(&headers, "unknown", |value| value == "1");
        assert!(!check);
    }

    #[test]
    fn test_check_header() {
        let headers = header_map(&[("test_header", "1")]);
        let check = check_header(&headers, "test_header", |value| value == "1");
        assert!(check);
        let check = check_header(&headers, "test_header", |value| value == "2");
        assert!(!check);
    }
}
