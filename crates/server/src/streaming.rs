//! Bounded native SSE delivery with disconnect cancellation.

use std::{convert::Infallible, time::Duration};

use axum::{
    Json,
    extract::{Extension, State, rejection::JsonRejection},
    http::HeaderMap,
    response::IntoResponse,
    response::sse::{Event, KeepAlive, Sse},
};
use imagegen_bridge_core::{ImageRequest, ProviderEvent};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

use crate::{
    ApiError, RequestId, ServerState, auth::AuthScope, routes::run_request_with_cancellation,
};

pub(crate) async fn stream_image(
    State(state): State<ServerState>,
    Extension(request_id): Extension<RequestId>,
    Extension(scope): Extension<AuthScope>,
    headers: HeaderMap,
    payload: Result<Json<ImageRequest>, JsonRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let Json(request) = payload.map_err(|_| {
        ApiError::bad_request(
            "request body must be valid ImageRequest JSON",
            request_id.clone(),
        )
    })?;
    let (sender, receiver) = mpsc::channel::<Result<Event, Infallible>>(4);
    sender
        .send(Ok(json_event("started", &ProviderEvent::Started)))
        .await
        .map_err(|_| ApiError::bad_request("SSE client disconnected", request_id.clone()))?;
    let cancellation = CancellationToken::new();
    let operation_cancellation = cancellation.clone();
    tokio::spawn(async move {
        let execution = run_request_with_cancellation(
            &state,
            request_id.clone(),
            scope,
            &headers,
            request,
            operation_cancellation,
        );
        tokio::select! {
            result = execution => {
                let event = match result {
                    Ok(response) => json_event(
                        "completed",
                        &ProviderEvent::Completed { response: Box::new(response) },
                    ),
                    Err(error) => json_event("error", &error.envelope()),
                };
                let _ = sender.send(Ok(event)).await;
            }
            () = sender.closed() => {
                cancellation.cancel();
            }
        }
    });
    Ok(Sse::new(ReceiverStream::new(receiver)).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("heartbeat"),
    ))
}

fn json_event(name: &'static str, value: &impl serde::Serialize) -> Event {
    match Event::default().event(name).json_data(value) {
        Ok(event) => event,
        Err(_) => Event::default()
            .event("error")
            .data("{\"error\":{\"message\":\"event serialization failed\",\"type\":\"api_error\",\"param\":null,\"code\":\"internal_error\",\"imagegen_bridge\":{\"code\":\"internal\",\"retryable\":false}},\"request_id\":\"unavailable\"}"),
    }
}
