#![warn(
    // Base lints.
    clippy::all,
    // Some pedantic lints.
    clippy::pedantic,
    // New lints which are cool.
    clippy::nursery,
)]
#![
    allow(
        // I don't care about this.
        clippy::module_name_repetitions,
        // Yo, the hell you should put
        // it in docs, if signature is clear as sky.
        clippy::missing_errors_doc,
    )
]

use std::{net::SocketAddr, str::FromStr};

use axum::{
    extract::{Extension, Request},
    middleware::{from_fn, map_request, Next},
    response::Response,
    routing::get,
    Router, ServiceExt,
};
use fern::{
    colors::{Color, ColoredLevelConfig},
    Dispatch,
};
use http::{header::HeaderName, HeaderValue, Method};
use prometheus::{Encoder, TextEncoder};
use tower::Layer;
use tower_http::{
    normalize_path::{NormalizePath, NormalizePathLayer},
    trace::TraceLayer,
};
use wildmatch::WildMatch;

use config::RustusConf;

use metrics::RustusMetrics;

use crate::{errors::RustusResult, server::rustus_service, state::State};

mod config;
mod data_storage;
mod errors;
mod file_info;
mod info_storage;
mod metrics;
mod notifiers;
mod protocol;
mod routes;
mod server;
mod state;
mod utils;

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn greeting(app_conf: &RustusConf) {
    let extensions = app_conf
        .tus_extensions
        .clone()
        .into_iter()
        .map(|x| x.to_string())
        .collect::<Vec<String>>()
        .join(", ");
    let hooks = app_conf
        .notification_opts
        .hooks
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<String>>()
        .join(", ");
    let rustus_logo = include_str!("../imgs/rustus_startup_logo.txt");
    eprintln!("\n\n{rustus_logo}");
    eprintln!("Welcome to rustus!");
    eprintln!("Base URL: /{}", app_conf.base_url());
    eprintln!("Available extensions: {extensions}");
    eprintln!("Enabled hooks: {hooks}");
    eprintln!();
    eprintln!();
}

/// Configuration for the CORS middleware.
///
/// It holds the pre-computed origin matchers and the
/// static header values that are attached to every
/// response.
#[derive(Clone)]
struct CorsConfig {
    matchers: Vec<WildMatch>,
    allow_headers: HeaderValue,
    expose_headers: HeaderValue,
}

/// Create CORS configuration for the server.
///
/// CORS rules are applied to every handler.
///
/// If the origins vector is empty all origins are
/// welcome, otherwise it will create a wildcard match for
/// every host.
fn create_cors(origins: Vec<String>, additional_headers: Vec<String>) -> CorsConfig {
    let mut allowed_headers = vec![
        "Content-Type".to_string(),
        "Upload-Offset".to_string(),
        "Upload-Checksum".to_string(),
        "Upload-Length".to_string(),
        "Upload-Metadata".to_string(),
        "Upload-Concat".to_string(),
        "Upload-Defer-Length".to_string(),
        "Tus-Resumable".to_string(),
        "Tus-Version".to_string(),
        "X-HTTP-Method-Override".to_string(),
        "Authorization".to_string(),
        "Origin".to_string(),
        "X-Requested-With".to_string(),
        "X-Request-ID".to_string(),
    ];
    for header in additional_headers {
        if HeaderName::from_str(&header).is_ok() {
            allowed_headers.push(header);
        }
    }

    let expose_headers = [
        "Location",
        "Tus-Version",
        "Tus-Resumable",
        "Tus-Max-Size",
        "Tus-Extension",
        "Tus-Checksum-Algorithm",
        "Content-Type",
        "Content-Length",
        "Upload-Length",
        "Upload-Metadata",
        "Upload-Defer-Length",
        "Upload-Concat",
        "Upload-Offset",
    ]
    .join(", ");

    let matchers = origins
        .into_iter()
        .map(|origin| WildMatch::new(&origin))
        .collect::<Vec<_>>();

    CorsConfig {
        matchers,
        allow_headers: HeaderValue::from_str(&allowed_headers.join(", "))
            .unwrap_or_else(|_| HeaderValue::from_static("")),
        expose_headers: HeaderValue::from_str(&expose_headers)
            .unwrap_or_else(|_| HeaderValue::from_static("")),
    }
}

/// Middleware that attaches CORS headers to every response.
///
/// Unlike `tower_http::cors::CorsLayer`, this middleware
/// forwards `OPTIONS` requests to the inner service so the
/// `server_info` handler can advertise the Tus extensions,
/// while still exposing the same CORS headers as before.
async fn cors_middleware(
    Extension(config): Extension<CorsConfig>,
    request: Request,
    next: Next,
) -> Response {
    let request_origin = request
        .headers()
        .get(http::header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string);
    let mut response = next.run(request).await;
    let headers = response.headers_mut();

    let allow_origin = if config.matchers.is_empty() {
        Some(HeaderValue::from_static("*"))
    } else if let Some(origin) = request_origin {
        if config
            .matchers
            .iter()
            .any(|matcher| *matcher == origin.as_str())
        {
            HeaderValue::from_str(&origin).ok()
        } else {
            None
        }
    } else {
        None
    };

    if let Some(origin) = allow_origin {
        headers.insert(http::header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        headers.insert(
            http::header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("OPTIONS, GET, HEAD, POST, PATCH, DELETE"),
        );
        headers.insert(
            http::header::ACCESS_CONTROL_ALLOW_HEADERS,
            config.allow_headers.clone(),
        );
        headers.insert(
            http::header::ACCESS_CONTROL_EXPOSE_HEADERS,
            config.expose_headers.clone(),
        );
        headers.insert(
            http::header::ACCESS_CONTROL_MAX_AGE,
            HeaderValue::from_static("86400"),
        );
    }

    response
}

/// Handler that exposes prometheus metrics.
///
/// It encodes the metrics registry into the
/// prometheus text exposition format.
#[allow(clippy::unused_async)]
async fn metrics_handler(Extension(metrics): Extension<RustusMetrics>) -> Response<String> {
    let encoder = TextEncoder::new();
    let metric_families = metrics.registry.gather();
    let mut buffer = Vec::new();
    if let Err(err) = encoder.encode(&metric_families, &mut buffer) {
        log::error!("{err}");
    }
    let body = String::from_utf8(buffer).unwrap_or_default();
    Response::builder()
        .status(http::StatusCode::OK)
        .header(http::header::CONTENT_TYPE, encoder.format_type())
        .body(body)
        .unwrap_or_default()
}

/// Middleware that overrides method of a request if
/// "X-HTTP-Method-Override" header is provided.
#[allow(clippy::unused_async)]
async fn method_override(mut request: Request) -> Request {
    if let Some(header_value) = request.headers().get("X-HTTP-Method-Override") {
        if let Ok(method_name) = header_value.to_str() {
            if let Ok(method) = Method::from_str(method_name) {
                *request.method_mut() = method;
            }
        }
    }
    request
}

/// Middleware that registers found errors.
///
/// It increments the `found_errors` counter for every
/// response that resolves to an error status code.
async fn error_metrics(
    Extension(metrics): Extension<RustusMetrics>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path().to_string();
    let response = next.run(request).await;
    if response.status().is_client_error() || response.status().is_server_error() {
        metrics
            .found_errors
            .with_label_values(&[path.as_str(), response.status().as_str()])
            .inc();
    }
    response
}

/// Builds the axum application router.
///
/// This router holds every route the server serves,
/// including the health check and metrics endpoints,
/// wrapped with all the required middleware.
fn build_router(state: State, metrics: RustusMetrics) -> NormalizePath<Router> {
    let cors_hosts = state.config.cors.clone();
    let proxy_headers = state
        .config
        .notification_opts
        .hooks_http_proxy_headers
        .clone();

    let cors_config = create_cors(cors_hosts, proxy_headers);

    let router = Router::new()
        .route("/health", get(routes::health_check))
        .route("/metrics", get(metrics_handler))
        .merge(rustus_service(state))
        .layer(TraceLayer::new_for_http())
        .layer(from_fn(error_metrics))
        .layer(from_fn(cors_middleware))
        .layer(Extension(cors_config))
        .layer(map_request(method_override))
        .layer(Extension(metrics))
        .fallback(routes::not_found);

    NormalizePathLayer::trim_trailing_slash().layer(router)
}

fn setup_logging(app_config: &RustusConf) -> RustusResult<()> {
    let colors = ColoredLevelConfig::new()
        // use builder methods
        .info(Color::Green)
        .warn(Color::Yellow)
        .debug(Color::BrightCyan)
        .error(Color::BrightRed)
        .trace(Color::Blue);

    Dispatch::new()
        .level(app_config.log_level)
        .chain(std::io::stdout())
        .format(move |out, message, record| {
            out.finish(format_args!(
                "{}[{}] {}",
                chrono::Local::now().format("[%Y-%m-%d][%H:%M:%S%:z]"),
                colors.color(record.level()),
                message
            ));
        })
        .apply()?;
    Ok(())
}

/// Main program entrypoint.
#[tokio::main]
async fn main() -> std::io::Result<()> {
    dotenvy::dotenv().ok();
    let app_conf = RustusConf::from_args();
    // Configuring logging.
    setup_logging(&app_conf)?;

    #[allow(clippy::collection_is_never_read)]
    let mut _guard = None;
    if let Some(dsn) = &app_conf.sentry_opts.dsn {
        log::info!("Setting up sentry .");
        _guard = Some(sentry::init((
            dsn.as_str(),
            sentry::ClientOptions {
                debug: true,
                sample_rate: app_conf.sentry_opts.sample_rate,
                ..Default::default()
            },
        )));
    }

    // Printing cool message.
    greeting(&app_conf);

    let host = app_conf.host.clone();
    let port = app_conf.port;

    let state = State::new(app_conf.clone()).await?;
    let metrics = RustusMetrics::new().map_err(std::io::Error::from)?;
    let app = build_router(state, metrics);

    let addr = SocketAddr::new(host.parse().expect("Invalid host"), port);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let make_service =
        ServiceExt::<Request>::into_make_service_with_connect_info::<SocketAddr>(app);
    axum::serve(listener, make_service).await
}
