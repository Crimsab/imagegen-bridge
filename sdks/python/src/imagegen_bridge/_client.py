"""HTTPX-backed sync and async clients for the native bridge API."""

from __future__ import annotations

import json
from collections.abc import AsyncIterator, Iterator, Mapping
from ipaddress import ip_address
from types import TracebackType
from typing import Any, cast
from urllib.parse import quote, urlsplit

import httpx

from ._errors import BridgeAPIError, BridgeProtocolError, BridgeTransportError
from ._sse import aiter_sse, iter_sse
from ._types import (
    ImageJob,
    ImageJobPage,
    ImageJobStatus,
    ImageJobVisibility,
    ImagePreset,
    ImagePresetCreate,
    ImagePresetPage,
    ImagePresetWrite,
    ImageRequest,
    ImageResponse,
    JSONValue,
    OperatorDiagnostics,
    ProviderCapabilities,
    ProviderPage,
    SessionMetadata,
    StreamEvent,
)

Timeout = float | httpx.Timeout | None


class _UseClientTimeout:
    __slots__ = ()


_USE_CLIENT_TIMEOUT = _UseClientTimeout()
TimeoutArg = Timeout | _UseClientTimeout
_MAX_PARTIAL_PREVIEW_BYTES = 16 * 1024 * 1024
_DEFAULT_MAX_RESPONSE_BODY_BYTES = 256 * 1024 * 1024
_DEFAULT_MAX_ERROR_BODY_BYTES = 1024 * 1024
_IMAGE_CONTENT_TYPES = {"image/png", "image/jpeg", "image/webp"}


def _decode_json(response: httpx.Response) -> dict[str, Any]:
    try:
        value = response.json()
    except (json.JSONDecodeError, UnicodeError) as error:
        raise BridgeProtocolError("bridge returned invalid JSON") from error
    if not isinstance(value, dict):
        raise BridgeProtocolError("bridge returned a non-object JSON response")
    return cast(dict[str, Any], value)


def _raise_api_error(response: httpx.Response) -> None:
    if response.is_success:
        return
    try:
        payload: object = response.json()
    except (json.JSONDecodeError, UnicodeError):
        payload = None
    raise BridgeAPIError.from_payload(response.status_code, payload)


def _decode_job(response: httpx.Response) -> ImageJob:
    try:
        return ImageJob.from_dict(_decode_json(response))
    except (KeyError, TypeError, ValueError) as error:
        raise BridgeProtocolError("bridge returned an invalid durable job") from error


def _decode_job_page(response: httpx.Response) -> ImageJobPage:
    try:
        return ImageJobPage.from_dict(_decode_json(response))
    except (KeyError, TypeError, ValueError) as error:
        raise BridgeProtocolError("bridge returned an invalid durable job page") from error


def _decode_preset(response: httpx.Response) -> ImagePreset:
    try:
        return ImagePreset.from_dict(_decode_json(response))
    except (KeyError, TypeError, ValueError) as error:
        raise BridgeProtocolError("bridge returned an invalid image preset") from error


def _decode_preset_page(response: httpx.Response) -> ImagePresetPage:
    try:
        return ImagePresetPage.from_dict(_decode_json(response))
    except (KeyError, TypeError, ValueError) as error:
        raise BridgeProtocolError("bridge returned an invalid image preset page") from error


def _decode_partial_preview(response: httpx.Response) -> bytes:
    content_type = response.headers.get("content-type", "").split(";", 1)[0].strip().lower()
    content = response.content
    if content_type not in _IMAGE_CONTENT_TYPES or not content:
        raise BridgeProtocolError("bridge returned an invalid partial image preview")
    if len(content) > _MAX_PARTIAL_PREVIEW_BYTES:
        raise BridgeProtocolError("bridge partial image preview exceeds the SDK limit")
    return content


def _validated_base_url(base_url: str, *, allow_insecure_remote_http: bool) -> str:
    parsed = urlsplit(base_url)
    host = parsed.hostname
    if (
        host is None
        or parsed.username is not None
        or parsed.password is not None
        or parsed.scheme not in {"http", "https"}
    ):
        raise ValueError("base_url must use HTTP(S), contain a host, and contain no user info")
    normalized_host = host.rstrip(".").lower()
    loopback = normalized_host == "localhost"
    if not loopback:
        try:
            address = ip_address(normalized_host)
            mapped = getattr(address, "ipv4_mapped", None)
            loopback = address.is_loopback or (mapped is not None and mapped.is_loopback)
        except ValueError:
            pass
    if parsed.scheme == "http" and not loopback and not allow_insecure_remote_http:
        raise ValueError("remote base_url must use HTTPS")
    return base_url.rstrip("/")


def _timeout_kwargs(timeout: TimeoutArg) -> dict[str, Any]:
    if timeout is _USE_CLIENT_TIMEOUT:
        return {}
    return {"timeout": cast(Timeout, timeout)}


def _response_with_content(response: httpx.Response, content: bytes) -> httpx.Response:
    return httpx.Response(
        response.status_code,
        headers=response.headers,
        content=content,
        request=response.request,
        extensions=response.extensions,
        history=response.history,
        default_encoding=response.default_encoding,
    )


def _validate_identity_encoding(response: httpx.Response) -> None:
    content_encoding = response.headers.get("content-encoding", "").strip().lower()
    if content_encoding not in {"", "identity"}:
        raise BridgeProtocolError("bridge response body uses unsupported content encoding")


def _validate_response_headers(response: httpx.Response, maximum_bytes: int) -> None:
    _validate_identity_encoding(response)
    content_length = response.headers.get("content-length")
    if content_length is None:
        return
    if not content_length.isascii() or not content_length.isdigit():
        raise BridgeProtocolError("bridge returned an invalid content length")
    declared_length = int(content_length)
    if declared_length > maximum_bytes:
        raise BridgeProtocolError("bridge response body exceeds the SDK limit")


def _read_limited_response(response: httpx.Response, maximum_bytes: int) -> httpx.Response:
    _validate_response_headers(response, maximum_bytes)
    if response.is_stream_consumed:
        if len(response.content) > maximum_bytes:
            raise BridgeProtocolError("bridge response body exceeds the SDK limit")
        return _response_with_content(response, response.content)
    content = bytearray()
    for chunk in response.iter_raw():
        if len(chunk) > maximum_bytes - len(content):
            raise BridgeProtocolError("bridge response body exceeds the SDK limit")
        content.extend(chunk)
    return _response_with_content(response, bytes(content))


async def _aread_limited_response(response: httpx.Response, maximum_bytes: int) -> httpx.Response:
    _validate_response_headers(response, maximum_bytes)
    if response.is_stream_consumed:
        if len(response.content) > maximum_bytes:
            raise BridgeProtocolError("bridge response body exceeds the SDK limit")
        return _response_with_content(response, response.content)
    content = bytearray()
    async for chunk in response.aiter_raw():
        if len(chunk) > maximum_bytes - len(content):
            raise BridgeProtocolError("bridge response body exceeds the SDK limit")
        content.extend(chunk)
    return _response_with_content(response, bytes(content))


def _job_update(favorite: bool | None, deleted: bool | None) -> dict[str, bool]:
    update = {}
    if favorite is not None:
        update["favorite"] = favorite
    if deleted is not None:
        update["deleted"] = deleted
    if not update:
        raise ValueError("job update requires favorite or deleted")
    return update


def _headers(bearer_token: str | None, default_headers: Mapping[str, str] | None) -> dict[str, str]:
    headers = {"accept": "application/json", "user-agent": "imagegen-bridge-python/0.1.1"}
    if default_headers:
        headers.update(default_headers)
    if bearer_token is not None:
        headers["authorization"] = f"Bearer {bearer_token}"
    headers["accept-encoding"] = "identity"
    return headers


def _request_headers(request: ImageRequest, idempotency_key: str | None) -> dict[str, str]:
    key = idempotency_key if idempotency_key is not None else request.idempotency_key
    return {"idempotency-key": key} if key is not None else {}


class AsyncImagesResource:
    def __init__(self, client: AsyncImagegenBridgeClient) -> None:
        self._client = client

    async def generate(
        self,
        request: ImageRequest,
        *,
        idempotency_key: str | None = None,
        timeout: TimeoutArg = _USE_CLIENT_TIMEOUT,
    ) -> ImageResponse:
        return await self._client._image(request, idempotency_key, timeout)

    async def edit(
        self,
        request: ImageRequest,
        *,
        idempotency_key: str | None = None,
        timeout: TimeoutArg = _USE_CLIENT_TIMEOUT,
    ) -> ImageResponse:
        if request.operation != "edit":
            raise ValueError("images.edit requires an edit ImageRequest")
        return await self._client._image(request, idempotency_key, timeout)

    async def stream(
        self,
        request: ImageRequest,
        *,
        idempotency_key: str | None = None,
        timeout: TimeoutArg = _USE_CLIENT_TIMEOUT,
    ) -> AsyncIterator[StreamEvent]:
        async for event in self._client._stream(request, idempotency_key, timeout):
            yield event


class ImagesResource:
    def __init__(self, client: ImagegenBridgeClient) -> None:
        self._client = client

    def generate(
        self,
        request: ImageRequest,
        *,
        idempotency_key: str | None = None,
        timeout: TimeoutArg = _USE_CLIENT_TIMEOUT,
    ) -> ImageResponse:
        return self._client._image(request, idempotency_key, timeout)

    def edit(
        self,
        request: ImageRequest,
        *,
        idempotency_key: str | None = None,
        timeout: TimeoutArg = _USE_CLIENT_TIMEOUT,
    ) -> ImageResponse:
        if request.operation != "edit":
            raise ValueError("images.edit requires an edit ImageRequest")
        return self._client._image(request, idempotency_key, timeout)

    def stream(
        self,
        request: ImageRequest,
        *,
        idempotency_key: str | None = None,
        timeout: TimeoutArg = _USE_CLIENT_TIMEOUT,
    ) -> Iterator[StreamEvent]:
        yield from self._client._stream(request, idempotency_key, timeout)


class AsyncJobsResource:
    def __init__(self, client: AsyncImagegenBridgeClient) -> None:
        self._client = client

    async def create(
        self,
        request: ImageRequest,
        *,
        idempotency_key: str | None = None,
        timeout: TimeoutArg = _USE_CLIENT_TIMEOUT,
    ) -> ImageJob:
        return await self._client._create_job(request, idempotency_key, timeout)

    async def get(self, job_id: str, *, timeout: TimeoutArg = _USE_CLIENT_TIMEOUT) -> ImageJob:
        return await self._client._get_job(job_id, timeout)

    async def list(
        self,
        *,
        limit: int = 20,
        cursor: str | None = None,
        status: ImageJobStatus | None = None,
        include_deleted: bool = False,
        visibility: ImageJobVisibility | None = None,
        favorite: bool | None = None,
        search: str | None = None,
        timeout: TimeoutArg = _USE_CLIENT_TIMEOUT,
    ) -> ImageJobPage:
        return await self._client._list_jobs(
            limit, cursor, status, include_deleted, visibility, favorite, search, timeout
        )

    async def cancel(self, job_id: str, *, timeout: TimeoutArg = _USE_CLIENT_TIMEOUT) -> ImageJob:
        return await self._client._cancel_job(job_id, timeout)

    async def partial(self, job_id: str, *, timeout: TimeoutArg = _USE_CLIENT_TIMEOUT) -> bytes:
        """Return the latest transient verified preview for a running job."""
        return await self._client._job_partial(job_id, timeout)

    async def update(
        self,
        job_id: str,
        *,
        favorite: bool | None = None,
        deleted: bool | None = None,
        timeout: TimeoutArg = _USE_CLIENT_TIMEOUT,
    ) -> ImageJob:
        return await self._client._update_job(job_id, favorite, deleted, timeout)


class JobsResource:
    def __init__(self, client: ImagegenBridgeClient) -> None:
        self._client = client

    def create(
        self,
        request: ImageRequest,
        *,
        idempotency_key: str | None = None,
        timeout: TimeoutArg = _USE_CLIENT_TIMEOUT,
    ) -> ImageJob:
        return self._client._create_job(request, idempotency_key, timeout)

    def get(self, job_id: str, *, timeout: TimeoutArg = _USE_CLIENT_TIMEOUT) -> ImageJob:
        return self._client._get_job(job_id, timeout)

    def list(
        self,
        *,
        limit: int = 20,
        cursor: str | None = None,
        status: ImageJobStatus | None = None,
        include_deleted: bool = False,
        visibility: ImageJobVisibility | None = None,
        favorite: bool | None = None,
        search: str | None = None,
        timeout: TimeoutArg = _USE_CLIENT_TIMEOUT,
    ) -> ImageJobPage:
        return self._client._list_jobs(
            limit, cursor, status, include_deleted, visibility, favorite, search, timeout
        )

    def cancel(self, job_id: str, *, timeout: TimeoutArg = _USE_CLIENT_TIMEOUT) -> ImageJob:
        return self._client._cancel_job(job_id, timeout)

    def partial(self, job_id: str, *, timeout: TimeoutArg = _USE_CLIENT_TIMEOUT) -> bytes:
        """Return the latest transient verified preview for a running job."""
        return self._client._job_partial(job_id, timeout)

    def update(
        self,
        job_id: str,
        *,
        favorite: bool | None = None,
        deleted: bool | None = None,
        timeout: TimeoutArg = _USE_CLIENT_TIMEOUT,
    ) -> ImageJob:
        return self._client._update_job(job_id, favorite, deleted, timeout)


class AsyncPresetsResource:
    def __init__(self, client: AsyncImagegenBridgeClient) -> None:
        self._client = client

    async def list(
        self,
        *,
        limit: int = 20,
        cursor: str | None = None,
        timeout: TimeoutArg = _USE_CLIENT_TIMEOUT,
    ) -> ImagePresetPage:
        return await self._client._list_presets(limit, cursor, timeout)

    async def get(self, name: str, *, timeout: TimeoutArg = _USE_CLIENT_TIMEOUT) -> ImagePreset:
        return await self._client._get_preset(name, timeout)

    async def create(
        self, preset: ImagePresetCreate, *, timeout: TimeoutArg = _USE_CLIENT_TIMEOUT
    ) -> ImagePreset:
        return await self._client._create_preset(preset, timeout)

    async def update(
        self,
        name: str,
        preset: ImagePresetWrite,
        *,
        timeout: TimeoutArg = _USE_CLIENT_TIMEOUT,
    ) -> ImagePreset:
        return await self._client._replace_preset(name, preset, timeout)

    async def delete(self, name: str, *, timeout: TimeoutArg = _USE_CLIENT_TIMEOUT) -> None:
        await self._client._delete_preset(name, timeout)


class PresetsResource:
    def __init__(self, client: ImagegenBridgeClient) -> None:
        self._client = client

    def list(
        self,
        *,
        limit: int = 20,
        cursor: str | None = None,
        timeout: TimeoutArg = _USE_CLIENT_TIMEOUT,
    ) -> ImagePresetPage:
        return self._client._list_presets(limit, cursor, timeout)

    def get(self, name: str, *, timeout: TimeoutArg = _USE_CLIENT_TIMEOUT) -> ImagePreset:
        return self._client._get_preset(name, timeout)

    def create(
        self, preset: ImagePresetCreate, *, timeout: TimeoutArg = _USE_CLIENT_TIMEOUT
    ) -> ImagePreset:
        return self._client._create_preset(preset, timeout)

    def update(
        self,
        name: str,
        preset: ImagePresetWrite,
        *,
        timeout: TimeoutArg = _USE_CLIENT_TIMEOUT,
    ) -> ImagePreset:
        return self._client._replace_preset(name, preset, timeout)

    def delete(self, name: str, *, timeout: TimeoutArg = _USE_CLIENT_TIMEOUT) -> None:
        self._client._delete_preset(name, timeout)


class AsyncImagegenBridgeClient:
    """Reusable async client. Close it or use it as an async context manager."""

    def __init__(
        self,
        base_url: str,
        *,
        bearer_token: str | None = None,
        timeout: Timeout = 60.0,
        max_sse_event_bytes: int = 4 * 1024 * 1024,
        max_sse_line_bytes: int = 4 * 1024 * 1024,
        max_response_body_bytes: int = _DEFAULT_MAX_RESPONSE_BODY_BYTES,
        max_error_body_bytes: int = _DEFAULT_MAX_ERROR_BODY_BYTES,
        allow_insecure_remote_http: bool = False,
        default_headers: Mapping[str, str] | None = None,
        transport: httpx.AsyncBaseTransport | None = None,
    ) -> None:
        for value, name in [
            (max_sse_event_bytes, "max_sse_event_bytes"),
            (max_sse_line_bytes, "max_sse_line_bytes"),
            (max_response_body_bytes, "max_response_body_bytes"),
            (max_error_body_bytes, "max_error_body_bytes"),
        ]:
            if value <= 0:
                raise ValueError(f"{name} must be positive")
        base_url = _validated_base_url(
            base_url, allow_insecure_remote_http=allow_insecure_remote_http
        )
        self._client = httpx.AsyncClient(
            base_url=base_url,
            headers=_headers(bearer_token, default_headers),
            timeout=timeout,
            transport=transport,
        )
        self._max_sse_event_bytes = max_sse_event_bytes
        self._max_sse_line_bytes = max_sse_line_bytes
        self._max_response_body_bytes = max_response_body_bytes
        self._max_error_body_bytes = max_error_body_bytes
        self.images = AsyncImagesResource(self)
        self.jobs = AsyncJobsResource(self)
        self.presets = AsyncPresetsResource(self)

    async def __aenter__(self) -> AsyncImagegenBridgeClient:
        return self

    async def __aexit__(
        self,
        exc_type: type[BaseException] | None,
        exc_value: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
        await self.close()

    async def close(self) -> None:
        await self._client.aclose()

    async def _image(
        self, request: ImageRequest, idempotency_key: str | None, timeout: TimeoutArg
    ) -> ImageResponse:
        response = await self._send(
            "POST",
            "/v1/images",
            json=request.to_dict(),
            headers=_request_headers(request, idempotency_key),
            **_timeout_kwargs(timeout),
        )
        try:
            return ImageResponse.from_dict(_decode_json(response))
        except (KeyError, TypeError, ValueError) as error:
            raise BridgeProtocolError("bridge returned an invalid image response") from error

    async def _stream(
        self, request: ImageRequest, idempotency_key: str | None, timeout: TimeoutArg
    ) -> AsyncIterator[StreamEvent]:
        try:
            async with self._client.stream(
                "POST",
                "/v1/images/stream",
                json=request.to_dict(),
                headers={
                    "accept": "text/event-stream",
                    **_request_headers(request, idempotency_key),
                },
                **_timeout_kwargs(timeout),
            ) as response:
                if not response.is_success:
                    limited = await _aread_limited_response(response, self._max_error_body_bytes)
                    _raise_api_error(limited)
                _validate_identity_encoding(response)
                async for event in aiter_sse(
                    response.aiter_bytes(),
                    self._max_sse_line_bytes,
                    self._max_sse_event_bytes,
                ):
                    yield event
        except httpx.HTTPError as error:
            raise BridgeTransportError("bridge streaming request failed") from error

    async def _create_job(
        self, request: ImageRequest, idempotency_key: str | None, timeout: TimeoutArg
    ) -> ImageJob:
        response = await self._send(
            "POST",
            "/v1/jobs",
            json=request.to_dict(),
            headers=_request_headers(request, idempotency_key),
            **_timeout_kwargs(timeout),
        )
        return _decode_job(response)

    async def _get_job(self, job_id: str, timeout: TimeoutArg) -> ImageJob:
        response = await self._send(
            "GET", f"/v1/jobs/{quote(job_id, safe='')}", **_timeout_kwargs(timeout)
        )
        return _decode_job(response)

    async def _list_jobs(
        self,
        limit: int,
        cursor: str | None,
        status: ImageJobStatus | None,
        include_deleted: bool,
        visibility: ImageJobVisibility | None,
        favorite: bool | None,
        search: str | None,
        timeout: TimeoutArg,
    ) -> ImageJobPage:
        if include_deleted and visibility is not None:
            raise ValueError("include_deleted cannot be combined with visibility")
        response = await self._send(
            "GET",
            "/v1/jobs",
            params={
                "limit": limit,
                "cursor": cursor,
                "status": status,
                "visibility": visibility,
                "favorite": None if favorite is None else str(favorite).lower(),
                "search": search,
                "include_deleted": "true" if include_deleted else None,
            },
            **_timeout_kwargs(timeout),
        )
        return _decode_job_page(response)

    async def _cancel_job(self, job_id: str, timeout: TimeoutArg) -> ImageJob:
        response = await self._send(
            "DELETE", f"/v1/jobs/{quote(job_id, safe='')}", **_timeout_kwargs(timeout)
        )
        return _decode_job(response)

    async def _job_partial(self, job_id: str, timeout: TimeoutArg) -> bytes:
        response = await self._send(
            "GET",
            f"/v1/jobs/{quote(job_id, safe='')}/partial",
            max_response_body_bytes=_MAX_PARTIAL_PREVIEW_BYTES,
            **_timeout_kwargs(timeout),
        )
        return _decode_partial_preview(response)

    async def _update_job(
        self,
        job_id: str,
        favorite: bool | None,
        deleted: bool | None,
        timeout: TimeoutArg,
    ) -> ImageJob:
        body = _job_update(favorite, deleted)
        response = await self._send(
            "PATCH",
            f"/v1/jobs/{quote(job_id, safe='')}",
            json=body,
            **_timeout_kwargs(timeout),
        )
        return _decode_job(response)

    async def _list_presets(
        self, limit: int, cursor: str | None, timeout: TimeoutArg
    ) -> ImagePresetPage:
        response = await self._send(
            "GET",
            "/v1/presets",
            params={"limit": limit, "cursor": cursor},
            **_timeout_kwargs(timeout),
        )
        return _decode_preset_page(response)

    async def _get_preset(self, name: str, timeout: TimeoutArg) -> ImagePreset:
        response = await self._send(
            "GET", f"/v1/presets/{quote(name, safe='')}", **_timeout_kwargs(timeout)
        )
        return _decode_preset(response)

    async def _create_preset(self, preset: ImagePresetCreate, timeout: TimeoutArg) -> ImagePreset:
        response = await self._send(
            "POST", "/v1/presets", json=preset.to_dict(), **_timeout_kwargs(timeout)
        )
        return _decode_preset(response)

    async def _replace_preset(
        self, name: str, preset: ImagePresetWrite, timeout: TimeoutArg
    ) -> ImagePreset:
        response = await self._send(
            "PUT",
            f"/v1/presets/{quote(name, safe='')}",
            json=preset.to_dict(),
            **_timeout_kwargs(timeout),
        )
        return _decode_preset(response)

    async def _delete_preset(self, name: str, timeout: TimeoutArg) -> None:
        await self._send(
            "DELETE", f"/v1/presets/{quote(name, safe='')}", **_timeout_kwargs(timeout)
        )

    async def providers(self, *, limit: int = 20, cursor: str | None = None) -> ProviderPage:
        response = await self._send(
            "GET", "/v1/providers", params={"limit": limit, "cursor": cursor}
        )
        try:
            return ProviderPage.from_dict(_decode_json(response))
        except (KeyError, TypeError, ValueError) as error:
            raise BridgeProtocolError("bridge returned an invalid provider page") from error

    async def capabilities(
        self, provider: str, *, model: str | None = None
    ) -> ProviderCapabilities:
        response = await self._send(
            "GET", f"/v1/providers/{quote(provider, safe='')}/capabilities", params={"model": model}
        )
        try:
            return ProviderCapabilities.from_dict(_decode_json(response))
        except (KeyError, TypeError, ValueError) as error:
            raise BridgeProtocolError("bridge returned invalid provider capabilities") from error

    async def diagnostics(self) -> OperatorDiagnostics:
        response = await self._send("GET", "/v1/diagnostics")
        try:
            return OperatorDiagnostics.from_dict(_decode_json(response))
        except (KeyError, TypeError, ValueError) as error:
            raise BridgeProtocolError("bridge returned invalid operator diagnostics") from error

    async def session(self, key: str, *, provider: str | None = None) -> SessionMetadata:
        response = await self._send(
            "GET", f"/v1/sessions/{quote(key, safe='')}", params={"provider": provider}
        )
        try:
            return SessionMetadata.from_dict(_decode_json(response))
        except (KeyError, TypeError, ValueError) as error:
            raise BridgeProtocolError("bridge returned invalid session metadata") from error

    async def delete_session(self, key: str, *, provider: str | None = None) -> None:
        await self._send(
            "DELETE", f"/v1/sessions/{quote(key, safe='')}", params={"provider": provider}
        )

    async def health(self, *, ready: bool = False) -> dict[str, JSONValue]:
        response = await self._send("GET", "/health/ready" if ready else "/health/live")
        return cast(dict[str, JSONValue], _decode_json(response))

    async def _send(
        self,
        method: str,
        path: str,
        *,
        max_response_body_bytes: int | None = None,
        **kwargs: Any,
    ) -> httpx.Response:
        try:
            request = self._client.build_request(method, path, **kwargs)
            response = await self._client.send(request, stream=True)
            try:
                maximum = (
                    self._max_error_body_bytes
                    if not response.is_success
                    else max_response_body_bytes or self._max_response_body_bytes
                )
                limited = await _aread_limited_response(response, maximum)
            finally:
                await response.aclose()
        except httpx.HTTPError as error:
            raise BridgeTransportError("bridge request failed") from error
        _raise_api_error(limited)
        return limited


class ImagegenBridgeClient:
    """Reusable blocking client. Close it or use it as a context manager."""

    def __init__(
        self,
        base_url: str,
        *,
        bearer_token: str | None = None,
        timeout: Timeout = 60.0,
        max_sse_event_bytes: int = 4 * 1024 * 1024,
        max_sse_line_bytes: int = 4 * 1024 * 1024,
        max_response_body_bytes: int = _DEFAULT_MAX_RESPONSE_BODY_BYTES,
        max_error_body_bytes: int = _DEFAULT_MAX_ERROR_BODY_BYTES,
        allow_insecure_remote_http: bool = False,
        default_headers: Mapping[str, str] | None = None,
        transport: httpx.BaseTransport | None = None,
    ) -> None:
        for value, name in [
            (max_sse_event_bytes, "max_sse_event_bytes"),
            (max_sse_line_bytes, "max_sse_line_bytes"),
            (max_response_body_bytes, "max_response_body_bytes"),
            (max_error_body_bytes, "max_error_body_bytes"),
        ]:
            if value <= 0:
                raise ValueError(f"{name} must be positive")
        base_url = _validated_base_url(
            base_url, allow_insecure_remote_http=allow_insecure_remote_http
        )
        self._client = httpx.Client(
            base_url=base_url,
            headers=_headers(bearer_token, default_headers),
            timeout=timeout,
            transport=transport,
        )
        self._max_sse_event_bytes = max_sse_event_bytes
        self._max_sse_line_bytes = max_sse_line_bytes
        self._max_response_body_bytes = max_response_body_bytes
        self._max_error_body_bytes = max_error_body_bytes
        self.images = ImagesResource(self)
        self.jobs = JobsResource(self)
        self.presets = PresetsResource(self)

    def __enter__(self) -> ImagegenBridgeClient:
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc_value: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
        self.close()

    def close(self) -> None:
        self._client.close()

    def _image(
        self, request: ImageRequest, idempotency_key: str | None, timeout: TimeoutArg
    ) -> ImageResponse:
        response = self._send(
            "POST",
            "/v1/images",
            json=request.to_dict(),
            headers=_request_headers(request, idempotency_key),
            **_timeout_kwargs(timeout),
        )
        try:
            return ImageResponse.from_dict(_decode_json(response))
        except (KeyError, TypeError, ValueError) as error:
            raise BridgeProtocolError("bridge returned an invalid image response") from error

    def _stream(
        self, request: ImageRequest, idempotency_key: str | None, timeout: TimeoutArg
    ) -> Iterator[StreamEvent]:
        try:
            with self._client.stream(
                "POST",
                "/v1/images/stream",
                json=request.to_dict(),
                headers={
                    "accept": "text/event-stream",
                    **_request_headers(request, idempotency_key),
                },
                **_timeout_kwargs(timeout),
            ) as response:
                if not response.is_success:
                    limited = _read_limited_response(response, self._max_error_body_bytes)
                    _raise_api_error(limited)
                _validate_identity_encoding(response)
                yield from iter_sse(
                    response.iter_bytes(),
                    self._max_sse_line_bytes,
                    self._max_sse_event_bytes,
                )
        except httpx.HTTPError as error:
            raise BridgeTransportError("bridge streaming request failed") from error

    def _create_job(
        self, request: ImageRequest, idempotency_key: str | None, timeout: TimeoutArg
    ) -> ImageJob:
        response = self._send(
            "POST",
            "/v1/jobs",
            json=request.to_dict(),
            headers=_request_headers(request, idempotency_key),
            **_timeout_kwargs(timeout),
        )
        return _decode_job(response)

    def _get_job(self, job_id: str, timeout: TimeoutArg) -> ImageJob:
        response = self._send(
            "GET", f"/v1/jobs/{quote(job_id, safe='')}", **_timeout_kwargs(timeout)
        )
        return _decode_job(response)

    def _list_jobs(
        self,
        limit: int,
        cursor: str | None,
        status: ImageJobStatus | None,
        include_deleted: bool,
        visibility: ImageJobVisibility | None,
        favorite: bool | None,
        search: str | None,
        timeout: TimeoutArg,
    ) -> ImageJobPage:
        if include_deleted and visibility is not None:
            raise ValueError("include_deleted cannot be combined with visibility")
        response = self._send(
            "GET",
            "/v1/jobs",
            params={
                "limit": limit,
                "cursor": cursor,
                "status": status,
                "visibility": visibility,
                "favorite": None if favorite is None else str(favorite).lower(),
                "search": search,
                "include_deleted": "true" if include_deleted else None,
            },
            **_timeout_kwargs(timeout),
        )
        return _decode_job_page(response)

    def _cancel_job(self, job_id: str, timeout: TimeoutArg) -> ImageJob:
        response = self._send(
            "DELETE", f"/v1/jobs/{quote(job_id, safe='')}", **_timeout_kwargs(timeout)
        )
        return _decode_job(response)

    def _job_partial(self, job_id: str, timeout: TimeoutArg) -> bytes:
        response = self._send(
            "GET",
            f"/v1/jobs/{quote(job_id, safe='')}/partial",
            max_response_body_bytes=_MAX_PARTIAL_PREVIEW_BYTES,
            **_timeout_kwargs(timeout),
        )
        return _decode_partial_preview(response)

    def _update_job(
        self,
        job_id: str,
        favorite: bool | None,
        deleted: bool | None,
        timeout: TimeoutArg,
    ) -> ImageJob:
        body = _job_update(favorite, deleted)
        response = self._send(
            "PATCH",
            f"/v1/jobs/{quote(job_id, safe='')}",
            json=body,
            **_timeout_kwargs(timeout),
        )
        return _decode_job(response)

    def _list_presets(self, limit: int, cursor: str | None, timeout: TimeoutArg) -> ImagePresetPage:
        response = self._send(
            "GET",
            "/v1/presets",
            params={"limit": limit, "cursor": cursor},
            **_timeout_kwargs(timeout),
        )
        return _decode_preset_page(response)

    def _get_preset(self, name: str, timeout: TimeoutArg) -> ImagePreset:
        response = self._send(
            "GET", f"/v1/presets/{quote(name, safe='')}", **_timeout_kwargs(timeout)
        )
        return _decode_preset(response)

    def _create_preset(self, preset: ImagePresetCreate, timeout: TimeoutArg) -> ImagePreset:
        response = self._send(
            "POST", "/v1/presets", json=preset.to_dict(), **_timeout_kwargs(timeout)
        )
        return _decode_preset(response)

    def _replace_preset(
        self, name: str, preset: ImagePresetWrite, timeout: TimeoutArg
    ) -> ImagePreset:
        response = self._send(
            "PUT",
            f"/v1/presets/{quote(name, safe='')}",
            json=preset.to_dict(),
            **_timeout_kwargs(timeout),
        )
        return _decode_preset(response)

    def _delete_preset(self, name: str, timeout: TimeoutArg) -> None:
        self._send("DELETE", f"/v1/presets/{quote(name, safe='')}", **_timeout_kwargs(timeout))

    def providers(self, *, limit: int = 20, cursor: str | None = None) -> ProviderPage:
        response = self._send("GET", "/v1/providers", params={"limit": limit, "cursor": cursor})
        try:
            return ProviderPage.from_dict(_decode_json(response))
        except (KeyError, TypeError, ValueError) as error:
            raise BridgeProtocolError("bridge returned an invalid provider page") from error

    def capabilities(self, provider: str, *, model: str | None = None) -> ProviderCapabilities:
        response = self._send(
            "GET", f"/v1/providers/{quote(provider, safe='')}/capabilities", params={"model": model}
        )
        try:
            return ProviderCapabilities.from_dict(_decode_json(response))
        except (KeyError, TypeError, ValueError) as error:
            raise BridgeProtocolError("bridge returned invalid provider capabilities") from error

    def diagnostics(self) -> OperatorDiagnostics:
        response = self._send("GET", "/v1/diagnostics")
        try:
            return OperatorDiagnostics.from_dict(_decode_json(response))
        except (KeyError, TypeError, ValueError) as error:
            raise BridgeProtocolError("bridge returned invalid operator diagnostics") from error

    def session(self, key: str, *, provider: str | None = None) -> SessionMetadata:
        response = self._send(
            "GET", f"/v1/sessions/{quote(key, safe='')}", params={"provider": provider}
        )
        try:
            return SessionMetadata.from_dict(_decode_json(response))
        except (KeyError, TypeError, ValueError) as error:
            raise BridgeProtocolError("bridge returned invalid session metadata") from error

    def delete_session(self, key: str, *, provider: str | None = None) -> None:
        self._send("DELETE", f"/v1/sessions/{quote(key, safe='')}", params={"provider": provider})

    def health(self, *, ready: bool = False) -> dict[str, JSONValue]:
        response = self._send("GET", "/health/ready" if ready else "/health/live")
        return cast(dict[str, JSONValue], _decode_json(response))

    def _send(
        self,
        method: str,
        path: str,
        *,
        max_response_body_bytes: int | None = None,
        **kwargs: Any,
    ) -> httpx.Response:
        try:
            request = self._client.build_request(method, path, **kwargs)
            response = self._client.send(request, stream=True)
            try:
                maximum = (
                    self._max_error_body_bytes
                    if not response.is_success
                    else max_response_body_bytes or self._max_response_body_bytes
                )
                limited = _read_limited_response(response, maximum)
            finally:
                response.close()
        except httpx.HTTPError as error:
            raise BridgeTransportError("bridge request failed") from error
        _raise_api_error(limited)
        return limited
