//! Durable asynchronous generation job contracts.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{BridgeError, ImageRequest, ImageResponse};

/// Durable lifecycle state for one asynchronous image operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ImageJobStatus {
    /// Accepted and waiting for bounded worker capacity.
    Queued,
    /// Claimed by a worker; provider completion may be ambiguous after a crash.
    Running,
    /// Completed with a verified response.
    Succeeded,
    /// Completed with a structured bridge error.
    Failed,
    /// Cancelled before or during execution.
    Cancelled,
    /// Process stopped while a paid provider call may have been in flight.
    Interrupted,
}

impl ImageJobStatus {
    /// Whether no more execution will occur without a new submission.
    #[must_use]
    pub const fn terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }
}

/// Bounded latest progress snapshot for a durable job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImageJobProgress {
    /// Stable stage label without prompt or image content.
    pub stage: String,
    /// Number of bounded partial-image events observed.
    pub partial_images: u32,
}

/// List-safe durable job state without request image bodies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImageJobSummary {
    /// Stable `UUIDv7` job identifier.
    pub id: String,
    /// Current durable lifecycle state.
    pub status: ImageJobStatus,
    /// Unix creation timestamp.
    pub created: u64,
    /// Unix timestamp of the last durable transition.
    pub updated: u64,
    /// Worker claim timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started: Option<u64>,
    /// Terminal transition timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed: Option<u64>,
    /// Latest bounded progress snapshot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<ImageJobProgress>,
    /// User-selected gallery favorite state.
    pub favorite: bool,
    /// Soft-delete timestamp; deleted jobs are hidden from ordinary lists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<u64>,
}

/// Complete durable job record returned by detail lookup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImageJob {
    /// List-safe lifecycle summary.
    #[serde(flatten)]
    pub summary: ImageJobSummary,
    /// Original normalized request retained for recovery and inspection.
    pub request: ImageRequest,
    /// Verified terminal result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<ImageResponse>,
    /// Structured terminal error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<BridgeError>,
    /// Whether cancellation has been durably requested.
    pub cancel_requested: bool,
}

/// Cursor-paginated job/history result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImageJobPage {
    /// Stable newest-first page.
    pub items: Vec<ImageJobSummary>,
    /// Opaque cursor for the next page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// Mutable gallery fields for one retained durable job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImageJobUpdate {
    /// Set or clear the favorite marker.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub favorite: Option<bool>,
    /// Soft-delete or restore a terminal history item.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<bool>,
}
