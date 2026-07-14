//! Provider-neutral domain contract for Imagegen Bridge.

mod capabilities;
mod error;
mod job;
mod negotiation;
mod parameters;
mod preset;
mod provider;
mod request;
mod response;
mod schema;
mod validation;

pub use capabilities::*;
pub use error::*;
pub use job::*;
pub use negotiation::*;
pub use parameters::*;
pub use preset::*;
pub use provider::*;
pub use request::*;
pub use response::*;
pub use schema::*;
pub use validation::*;

/// Current native wire-contract version.
pub const CONTRACT_VERSION: &str = "1";
