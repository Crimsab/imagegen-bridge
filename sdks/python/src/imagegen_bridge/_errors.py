"""Structured SDK errors with no authentication or request-body reflection."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, cast

from ._types import JSONValue


class ImagegenBridgeError(Exception):
    """Base class for all SDK-defined failures."""


@dataclass(eq=False)
class BridgeAPIError(ImagegenBridgeError):
    """A structured non-success response or SSE error event from the bridge."""

    message: str
    status_code: int
    type: str = "api_error"
    code: str | None = None
    param: str | None = None
    bridge_code: str | None = None
    retryable: bool = False
    provider: str | None = None
    upstream_request_id: str | None = None
    request_id: str | None = None
    details: dict[str, JSONValue] = field(default_factory=dict)

    def __str__(self) -> str:
        discriminator = self.bridge_code or self.code or self.type
        return f"{discriminator}: {self.message}"

    @classmethod
    def from_payload(cls, status_code: int, payload: object) -> BridgeAPIError:
        if not isinstance(payload, dict):
            return cls("bridge returned an invalid error envelope", status_code)
        root = cast(dict[str, Any], payload)
        error = root.get("error")
        if not isinstance(error, dict):
            return cls(
                "bridge returned an invalid error envelope",
                status_code,
                request_id=_optional_string(root.get("request_id")),
            )
        extension = error.get("imagegen_bridge")
        bridge = extension if isinstance(extension, dict) else {}
        details = bridge.get("details")
        return cls(
            message=str(error.get("message", "bridge request failed")),
            status_code=status_code,
            type=str(error.get("type", "api_error")),
            code=_optional_string(error.get("code")),
            param=_optional_string(error.get("param")),
            bridge_code=_optional_string(bridge.get("code")),
            retryable=bridge.get("retryable") is True,
            provider=_optional_string(bridge.get("provider")),
            upstream_request_id=_optional_string(bridge.get("upstream_request_id")),
            request_id=_optional_string(root.get("request_id")),
            details=cast(dict[str, JSONValue], details) if isinstance(details, dict) else {},
        )


class BridgeProtocolError(ImagegenBridgeError):
    """The server returned a response that violates the advertised wire contract."""


class BridgeTransportError(ImagegenBridgeError):
    """The request could not reach or complete against the bridge."""


def _optional_string(value: object) -> str | None:
    return value if isinstance(value, str) else None
