use axum::{
    routing::{on, MethodFilter, MethodRouter},
    Router,
};

use crate::{RustusConf, State as RustusState};

mod core;
mod creation;
pub mod extensions;
mod getting;
mod termination;

/// Configure TUS web application.
///
/// This function resolves all protocol extensions
/// provided by CLI into a router and adds their routes to the application.
#[must_use]
pub fn setup(config: &RustusConf) -> Router<RustusState> {
    // Core endpoints are always available.
    // OPTIONS /    -> server info.
    let mut root: MethodRouter<RustusState> = on(MethodFilter::OPTIONS, core::server_info);
    // PATCH /{file_id}/ -> add bytes.
    // HEAD  /{file_id}/ -> file info.
    let mut file: MethodRouter<RustusState> =
        on(MethodFilter::PATCH, core::write_bytes).on(MethodFilter::HEAD, core::get_file_info);

    for extension in &config.tus_extensions {
        match extension {
            extensions::Extensions::Creation => {
                root = root.on(MethodFilter::POST, creation::create_file);
            }
            extensions::Extensions::Getting => {
                file = file.on(MethodFilter::GET, getting::get_file);
            }
            extensions::Extensions::Termination => {
                file = file.on(MethodFilter::DELETE, termination::terminate);
            }
            _ => {}
        }
    }

    Router::new().route("/", root).route("/{file_id}", file)
}
