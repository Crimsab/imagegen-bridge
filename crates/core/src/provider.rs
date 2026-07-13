//! Provider extension interface.

use std::{pin::Pin, time::Instant};

use async_trait::async_trait;
use futures_core::Stream;
use tokio_util::sync::CancellationToken;

use crate::{
    BridgeError, ImageRequest, ImageResponse, ProviderCapabilities, ProviderDescriptor,
    ProviderEvent,
};

/// Stream returned by providers that expose incremental progress.
pub type ProviderEventStream =
    Pin<Box<dyn Stream<Item = Result<ProviderEvent, BridgeError>> + Send + 'static>>;

/// Runtime metadata and cancellation propagated into a provider call.
#[derive(Debug, Clone)]
pub struct ProviderContext {
    /// Safe bridge request ID.
    pub request_id: String,
    /// Absolute request deadline.
    pub deadline: Instant,
    /// Cooperative cancellation signal.
    pub cancellation: CancellationToken,
}

/// Provider-neutral image backend.
#[async_trait]
pub trait ImageProvider: Send + Sync {
    /// Returns stable provider identity.
    fn descriptor(&self) -> ProviderDescriptor;

    /// Returns current capabilities for an optional model.
    async fn capabilities(&self, model: Option<&str>) -> Result<ProviderCapabilities, BridgeError>;

    /// Executes one normalized request without exposing incremental events.
    async fn execute(
        &self,
        request: ImageRequest,
        context: ProviderContext,
    ) -> Result<ImageResponse, BridgeError>;

    /// Starts a request with incremental events when supported.
    async fn execute_stream(
        &self,
        _request: ImageRequest,
        _context: ProviderContext,
    ) -> Result<ProviderEventStream, BridgeError> {
        Err(BridgeError::new(
            crate::ErrorCode::UnsupportedCapability,
            "provider does not support streaming image events",
        ))
    }

    /// Performs a non-generating auth/readiness check.
    async fn check_ready(&self) -> Result<(), BridgeError>;

    /// Releases provider resources during graceful shutdown.
    async fn shutdown(&self) -> Result<(), BridgeError> {
        Ok(())
    }
}
