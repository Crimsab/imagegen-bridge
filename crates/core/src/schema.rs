//! Schema root for the complete native wire contract.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    BridgeError, ImageRequest, ImageResponse, ProviderCapabilities, ProviderDescriptor,
    ProviderEvent, ValidationIssue,
};

/// All top-level native messages represented as one versioned JSON Schema.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "message", content = "body", rename_all = "snake_case")]
pub enum ContractMessage {
    /// Normalized request.
    Request(ImageRequest),
    /// Normalized response.
    Response(ImageResponse),
    /// Provider capability document.
    Capabilities(ProviderCapabilities),
    /// Provider discovery document.
    Provider(ProviderDescriptor),
    /// Incremental provider event.
    Event(ProviderEvent),
    /// Public bridge error.
    Error(BridgeError),
    /// One validation issue.
    ValidationIssue(ValidationIssue),
}

/// Generates the current native contract schema.
#[must_use]
pub fn contract_schema() -> schemars::Schema {
    schemars::schema_for!(ContractMessage)
}
