"""HTTPX-backed sync and async clients for the native bridge API."""

from __future__ import annotations

import json
from collections.abc import AsyncIterator, Iterator, Mapping
from types import TracebackType
from typing import Any, cast
from urllib.parse import quote

import httpx

from ._errors import BridgeAPIError, BridgeProtocolError, BridgeTransportError
from ._sse import aiter_sse, iter_sse
from ._types import (
    ImageJob,
    ImageJobPage,
    ImageJobStatus,
    ImageRequest,
    ImageResponse,
    JSONValue,
    ProviderCapabilities,
    ProviderPage,
    SessionMetadata,
    StreamEvent,
)

Timeout = float | httpx.Timeout | None


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


def _headers(bearer_token: str | None, default_headers: Mapping[str, str] | None) -> dict[str, str]:
    headers = {"accept": "application/json", "user-agent": "imagegen-bridge-python/0.1.0"}
    if default_headers:
        headers.update(default_headers)
    if bearer_token is not None:
        headers["authorization"] = f"Bearer {bearer_token}"
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
        timeout: Timeout = None,
    ) -> ImageResponse:
        return await self._client._image(request, idempotency_key, timeout)

    async def edit(
        self,
        request: ImageRequest,
        *,
        idempotency_key: str | None = None,
        timeout: Timeout = None,
    ) -> ImageResponse:
        if request.operation != "edit":
            raise ValueError("images.edit requires an edit ImageRequest")
        return await self._client._image(request, idempotency_key, timeout)

    async def stream(
        self,
        request: ImageRequest,
        *,
        idempotency_key: str | None = None,
        timeout: Timeout = None,
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
        timeout: Timeout = None,
    ) -> ImageResponse:
        return self._client._image(request, idempotency_key, timeout)

    def edit(
        self,
        request: ImageRequest,
        *,
        idempotency_key: str | None = None,
        timeout: Timeout = None,
    ) -> ImageResponse:
        if request.operation != "edit":
            raise ValueError("images.edit requires an edit ImageRequest")
        return self._client._image(request, idempotency_key, timeout)

    def stream(
        self,
        request: ImageRequest,
        *,
        idempotency_key: str | None = None,
        timeout: Timeout = None,
    ) -> Iterator[StreamEvent]:
        yield from self._client._stream(request, idempotency_key, timeout)


class AsyncJobsResource:
    def __init__(self, client: AsyncImagegenBridgeClient) -> None:
        self._client = client

    async def create(self, request: ImageRequest, *, timeout: Timeout = None) -> ImageJob:
        return await self._client._create_job(request, timeout)

    async def get(self, job_id: str, *, timeout: Timeout = None) -> ImageJob:
        return await self._client._get_job(job_id, timeout)

    async def list(
        self,
        *,
        limit: int = 20,
        cursor: str | None = None,
        status: ImageJobStatus | None = None,
        include_deleted: bool = False,
        timeout: Timeout = None,
    ) -> ImageJobPage:
        return await self._client._list_jobs(limit, cursor, status, include_deleted, timeout)

    async def cancel(self, job_id: str, *, timeout: Timeout = None) -> ImageJob:
        return await self._client._cancel_job(job_id, timeout)


class JobsResource:
    def __init__(self, client: ImagegenBridgeClient) -> None:
        self._client = client

    def create(self, request: ImageRequest, *, timeout: Timeout = None) -> ImageJob:
        return self._client._create_job(request, timeout)

    def get(self, job_id: str, *, timeout: Timeout = None) -> ImageJob:
        return self._client._get_job(job_id, timeout)

    def list(
        self,
        *,
        limit: int = 20,
        cursor: str | None = None,
        status: ImageJobStatus | None = None,
        include_deleted: bool = False,
        timeout: Timeout = None,
    ) -> ImageJobPage:
        return self._client._list_jobs(limit, cursor, status, include_deleted, timeout)

    def cancel(self, job_id: str, *, timeout: Timeout = None) -> ImageJob:
        return self._client._cancel_job(job_id, timeout)


class AsyncImagegenBridgeClient:
    """Reusable async client. Close it or use it as an async context manager."""

    def __init__(
        self,
        base_url: str,
        *,
        bearer_token: str | None = None,
        timeout: Timeout = 60.0,
        max_sse_event_bytes: int = 4 * 1024 * 1024,
        default_headers: Mapping[str, str] | None = None,
        transport: httpx.AsyncBaseTransport | None = None,
    ) -> None:
        if max_sse_event_bytes <= 0:
            raise ValueError("max_sse_event_bytes must be positive")
        self._client = httpx.AsyncClient(
            base_url=base_url.rstrip("/"),
            headers=_headers(bearer_token, default_headers),
            timeout=timeout,
            transport=transport,
        )
        self._max_sse_event_bytes = max_sse_event_bytes
        self.images = AsyncImagesResource(self)
        self.jobs = AsyncJobsResource(self)

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
        self, request: ImageRequest, idempotency_key: str | None, timeout: Timeout
    ) -> ImageResponse:
        response = await self._send(
            "POST",
            "/v1/images",
            json=request.to_dict(),
            headers=_request_headers(request, idempotency_key),
            timeout=timeout,
        )
        try:
            return ImageResponse.from_dict(_decode_json(response))
        except (KeyError, TypeError, ValueError) as error:
            raise BridgeProtocolError("bridge returned an invalid image response") from error

    async def _stream(
        self, request: ImageRequest, idempotency_key: str | None, timeout: Timeout
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
                timeout=timeout,
            ) as response:
                if not response.is_success:
                    await response.aread()
                    _raise_api_error(response)
                async for event in aiter_sse(response.aiter_lines(), self._max_sse_event_bytes):
                    yield event
        except httpx.HTTPError as error:
            raise BridgeTransportError("bridge streaming request failed") from error

    async def _create_job(self, request: ImageRequest, timeout: Timeout) -> ImageJob:
        response = await self._send("POST", "/v1/jobs", json=request.to_dict(), timeout=timeout)
        return _decode_job(response)

    async def _get_job(self, job_id: str, timeout: Timeout) -> ImageJob:
        response = await self._send("GET", f"/v1/jobs/{quote(job_id, safe='')}", timeout=timeout)
        return _decode_job(response)

    async def _list_jobs(
        self,
        limit: int,
        cursor: str | None,
        status: ImageJobStatus | None,
        include_deleted: bool,
        timeout: Timeout,
    ) -> ImageJobPage:
        response = await self._send(
            "GET",
            "/v1/jobs",
            params={
                "limit": limit,
                "cursor": cursor,
                "status": status,
                "include_deleted": str(include_deleted).lower(),
            },
            timeout=timeout,
        )
        return _decode_job_page(response)

    async def _cancel_job(self, job_id: str, timeout: Timeout) -> ImageJob:
        response = await self._send("DELETE", f"/v1/jobs/{quote(job_id, safe='')}", timeout=timeout)
        return _decode_job(response)

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

    async def _send(self, method: str, path: str, **kwargs: Any) -> httpx.Response:
        try:
            response = await self._client.request(method, path, **kwargs)
        except httpx.HTTPError as error:
            raise BridgeTransportError("bridge request failed") from error
        _raise_api_error(response)
        return response


class ImagegenBridgeClient:
    """Reusable blocking client. Close it or use it as a context manager."""

    def __init__(
        self,
        base_url: str,
        *,
        bearer_token: str | None = None,
        timeout: Timeout = 60.0,
        max_sse_event_bytes: int = 4 * 1024 * 1024,
        default_headers: Mapping[str, str] | None = None,
        transport: httpx.BaseTransport | None = None,
    ) -> None:
        if max_sse_event_bytes <= 0:
            raise ValueError("max_sse_event_bytes must be positive")
        self._client = httpx.Client(
            base_url=base_url.rstrip("/"),
            headers=_headers(bearer_token, default_headers),
            timeout=timeout,
            transport=transport,
        )
        self._max_sse_event_bytes = max_sse_event_bytes
        self.images = ImagesResource(self)
        self.jobs = JobsResource(self)

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
        self, request: ImageRequest, idempotency_key: str | None, timeout: Timeout
    ) -> ImageResponse:
        response = self._send(
            "POST",
            "/v1/images",
            json=request.to_dict(),
            headers=_request_headers(request, idempotency_key),
            timeout=timeout,
        )
        try:
            return ImageResponse.from_dict(_decode_json(response))
        except (KeyError, TypeError, ValueError) as error:
            raise BridgeProtocolError("bridge returned an invalid image response") from error

    def _stream(
        self, request: ImageRequest, idempotency_key: str | None, timeout: Timeout
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
                timeout=timeout,
            ) as response:
                if not response.is_success:
                    response.read()
                    _raise_api_error(response)
                yield from iter_sse(response.iter_lines(), self._max_sse_event_bytes)
        except httpx.HTTPError as error:
            raise BridgeTransportError("bridge streaming request failed") from error

    def _create_job(self, request: ImageRequest, timeout: Timeout) -> ImageJob:
        response = self._send("POST", "/v1/jobs", json=request.to_dict(), timeout=timeout)
        return _decode_job(response)

    def _get_job(self, job_id: str, timeout: Timeout) -> ImageJob:
        response = self._send("GET", f"/v1/jobs/{quote(job_id, safe='')}", timeout=timeout)
        return _decode_job(response)

    def _list_jobs(
        self,
        limit: int,
        cursor: str | None,
        status: ImageJobStatus | None,
        include_deleted: bool,
        timeout: Timeout,
    ) -> ImageJobPage:
        response = self._send(
            "GET",
            "/v1/jobs",
            params={
                "limit": limit,
                "cursor": cursor,
                "status": status,
                "include_deleted": str(include_deleted).lower(),
            },
            timeout=timeout,
        )
        return _decode_job_page(response)

    def _cancel_job(self, job_id: str, timeout: Timeout) -> ImageJob:
        response = self._send("DELETE", f"/v1/jobs/{quote(job_id, safe='')}", timeout=timeout)
        return _decode_job(response)

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

    def _send(self, method: str, path: str, **kwargs: Any) -> httpx.Response:
        try:
            response = self._client.request(method, path, **kwargs)
        except httpx.HTTPError as error:
            raise BridgeTransportError("bridge request failed") from error
        _raise_api_error(response)
        return response
