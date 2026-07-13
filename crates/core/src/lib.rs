//! Provider-neutral domain contract for Imagegen Bridge.

mod capabilities;
mod error;
mod parameters;
mod provider;
mod request;
mod response;
mod schema;
mod validation;

pub use capabilities::*;
pub use error::*;
pub use parameters::*;
pub use provider::*;
pub use request::*;
pub use response::*;
pub use schema::*;
pub use validation::*;

/// Current native wire-contract version.
pub const CONTRACT_VERSION: &str = "1";
