use axum::{extract::DefaultBodyLimit, Router};
use tower_http::set_header::SetResponseHeaderLayer;

use crate::{protocol, State};

pub fn rustus_service(state: State) -> Router {
    let base = format!("/{}", state.config.base_url());
    let max_body_size = state.config.max_body_size;
    Router::new()
        .nest(&base, protocol::setup(&state.config))
        .layer(SetResponseHeaderLayer::if_not_present(
            http::header::HeaderName::from_static("tus-resumable"),
            http::HeaderValue::from_static("1.0.0"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            http::header::HeaderName::from_static("tus-version"),
            http::HeaderValue::from_static("1.0.0"),
        ))
        .layer(DefaultBodyLimit::max(max_body_size))
        .with_state(state)
}

#[cfg(test)]
pub mod test {
    use super::rustus_service;
    use crate::{metrics::RustusMetrics, state::State};
    use axum::{Extension, Router};
    use tower::Layer;
    use tower_http::normalize_path::{NormalizePath, NormalizePathLayer};

    pub fn get_service(state: State) -> NormalizePath<Router> {
        let metrics = RustusMetrics::new().unwrap();
        let router = rustus_service(state).layer(Extension(metrics));
        NormalizePathLayer::trim_trailing_slash().layer(router)
    }
}
