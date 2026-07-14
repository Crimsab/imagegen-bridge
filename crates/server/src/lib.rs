//! Bounded HTTP transport over the shared Imagegen Bridge runtime.

mod auth;
mod compat;
mod dashboard;
mod diagnostics;
mod error;
mod events;
mod jobs;
mod metrics;
mod openapi;
mod readiness;
mod routes;
mod serve;
mod streaming;

pub use dashboard::dashboard_router;
pub use error::*;
pub use jobs::*;
pub use openapi::*;
pub use routes::*;
pub use serve::*;
