"""Bounded SSE decoders for sync and async HTTPX line iterators."""

from __future__ import annotations

import json
from collections.abc import AsyncIterable, AsyncIterator, Iterable, Iterator
from typing import Any

from ._errors import BridgeAPIError, BridgeProtocolError
from ._types import (
    CompletedEvent,
    ImageResponse,
    PartialImageEvent,
    ProgressEvent,
    StartedEvent,
    StreamEvent,
)


def _decode(event_name: str, data: str) -> StreamEvent:
    try:
        value: Any = json.loads(data)
    except (json.JSONDecodeError, UnicodeError) as error:
        raise BridgeProtocolError("bridge returned invalid JSON in an SSE event") from error
    if event_name == "error":
        raise BridgeAPIError.from_payload(200, value)
    if not isinstance(value, dict):
        raise BridgeProtocolError("bridge returned a non-object SSE event")
    event_type = value.get("type")
    if event_type == "started":
        return StartedEvent()
    if event_type == "progress" and isinstance(value.get("stage"), str):
        return ProgressEvent(stage=value["stage"])
    if event_type == "partial_image":
        try:
            return PartialImageEvent(
                index=int(value["index"]),
                partial_index=int(value["partial_index"]),
                b64_json=str(value["b64_json"]),
            )
        except (KeyError, TypeError, ValueError) as error:
            raise BridgeProtocolError("bridge returned an invalid partial-image event") from error
    if event_type == "completed" and isinstance(value.get("response"), dict):
        try:
            return CompletedEvent(ImageResponse.from_dict(value["response"]))
        except (KeyError, TypeError, ValueError) as error:
            raise BridgeProtocolError("bridge returned an invalid completion event") from error
    raise BridgeProtocolError(f"bridge returned unsupported SSE event type {event_type!r}")


def iter_sse(lines: Iterable[str], maximum_event_bytes: int) -> Iterator[StreamEvent]:
    event_name = "message"
    data: list[str] = []
    size = 0
    for line in lines:
        if line == "":
            if data:
                yield _decode(event_name, "\n".join(data))
            event_name, data, size = "message", [], 0
            continue
        if line.startswith(":"):
            continue
        size += len(line.encode("utf-8"))
        if size > maximum_event_bytes:
            raise BridgeProtocolError("bridge SSE event exceeded the configured byte limit")
        field, separator, value = line.partition(":")
        if separator and value.startswith(" "):
            value = value[1:]
        if field == "event":
            event_name = value
        elif field == "data":
            data.append(value)
    if data:
        yield _decode(event_name, "\n".join(data))


async def aiter_sse(
    lines: AsyncIterable[str], maximum_event_bytes: int
) -> AsyncIterator[StreamEvent]:
    event_name = "message"
    data: list[str] = []
    size = 0
    async for line in lines:
        if line == "":
            if data:
                yield _decode(event_name, "\n".join(data))
            event_name, data, size = "message", [], 0
            continue
        if line.startswith(":"):
            continue
        size += len(line.encode("utf-8"))
        if size > maximum_event_bytes:
            raise BridgeProtocolError("bridge SSE event exceeded the configured byte limit")
        field, separator, value = line.partition(":")
        if separator and value.startswith(" "):
            value = value[1:]
        if field == "event":
            event_name = value
        elif field == "data":
            data.append(value)
    if data:
        yield _decode(event_name, "\n".join(data))
