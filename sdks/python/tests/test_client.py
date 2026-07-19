from __future__ import annotations

import asyncio

import httpx
import pytest

from imagegen_bridge import (
    AsyncImagegenBridgeClient,
    BridgeAPIError,
    BridgeProtocolError,
    CompletedEvent,
    ImagegenBridgeClient,
    ImagePresetCreate,
    ImagePresetTemplate,
    ImagePresetWrite,
    ImageRequest,
    PartialImageEvent,
    ProgressEvent,
    StartedEvent,
)
from imagegen_bridge._sse import iter_sse

TOKEN = "sdk-test-token"


def test_rejects_plaintext_remote_urls_before_transport_use() -> None:
    called = False

    def handler(request: httpx.Request) -> httpx.Response:
        nonlocal called
        called = True
        return httpx.Response(
            200,
            request=request,
            headers={"content-type": "application/json"},
            stream=httpx.ByteStream(b'{"status":"live"}'),
        )

    transport = httpx.MockTransport(handler)
    with pytest.raises(ValueError, match="must use HTTPS"):
        ImagegenBridgeClient("http://10.0.0.2:8787", transport=transport)
    with pytest.raises(ValueError, match="must use HTTPS"):
        AsyncImagegenBridgeClient("http://bridge.example", transport=transport)
    assert not called

    with ImagegenBridgeClient(
        "http://10.0.0.2:8787",
        transport=transport,
        allow_insecure_remote_http=True,
    ) as client:
        assert client.health()["status"] == "live"
    assert called


def test_response_body_and_sse_line_limits_precede_decoding() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, request=request, content=b'{"status":"live"}')

    transport = httpx.MockTransport(handler)
    with (
        ImagegenBridgeClient(
            "https://bridge.example", transport=transport, max_response_body_bytes=8
        ) as client,
        pytest.raises(BridgeProtocolError, match="response body exceeds"),
    ):
        client.health()

    async def scenario() -> None:
        async with AsyncImagegenBridgeClient(
            "https://bridge.example", transport=transport, max_response_body_bytes=8
        ) as client:
            with pytest.raises(BridgeProtocolError, match="response body exceeds"):
                await client.health()

    asyncio.run(scenario())
    with pytest.raises(BridgeProtocolError, match="SSE line exceeded"):
        list(iter_sse([b":" + b"x" * 9], 8, 32))


def test_omitted_timeout_inherits_client_and_explicit_none_disables_it() -> None:
    observed: list[dict[str, float | None]] = []

    def handler(request: httpx.Request) -> httpx.Response:
        observed.append(request.extensions["timeout"])
        return httpx.Response(
            429,
            request=request,
            headers={"content-type": "application/json"},
            content=b'{"error":{"message":"limited","imagegen_bridge":{"code":"rate_limited"}}}',
        )

    with ImagegenBridgeClient(
        "https://bridge.example", transport=httpx.MockTransport(handler), timeout=7.0
    ) as client:
        with pytest.raises(BridgeAPIError):
            client.images.generate(ImageRequest.generate("inherit"))
        with pytest.raises(BridgeAPIError):
            client.images.generate(ImageRequest.generate("disable"), timeout=None)
    assert observed[0]["read"] == 7.0
    assert observed[1]["read"] is None

    async def scenario() -> None:
        async with AsyncImagegenBridgeClient(
            "https://bridge.example", transport=httpx.MockTransport(handler), timeout=9.0
        ) as client:
            with pytest.raises(BridgeAPIError):
                await client.jobs.create(ImageRequest.generate("async inherit"))
            with pytest.raises(BridgeAPIError):
                await client.jobs.create(ImageRequest.generate("async disable"), timeout=None)

    asyncio.run(scenario())
    assert observed[2]["read"] == 9.0
    assert observed[3]["read"] is None


def test_sync_client_matches_shared_http_contract(
    bridge_url: str,
    fixture_request: dict[str, object],
    edit_fixture_request: dict[str, object],
) -> None:
    request = ImageRequest.from_dict(fixture_request)
    assert request.to_dict() == fixture_request

    with ImagegenBridgeClient(bridge_url, bearer_token=TOKEN) as client:
        result = client.images.generate(request)
        assert result.id == "img_fixture_01"
        assert result.data[0].width == 1
        assert result.data[0].index == 0
        assert result.requested.failure_policy == "fail_fast"
        assert result.session is not None and result.session.reused
        assert result.normalizations[0].field == "parameters.partial_images"
        edited = client.images.edit(ImageRequest.from_dict(edit_fixture_request))
        assert edited.id == "img_fixture_01"

        events = list(client.images.stream(request))
        assert isinstance(events[0], StartedEvent)
        assert isinstance(events[1], ProgressEvent)
        assert isinstance(events[2], PartialImageEvent)
        assert isinstance(events[3], CompletedEvent)

        providers = client.providers(limit=2)
        assert [provider.name for provider in providers.items] == [
            "codex-app-server",
            "codex-responses",
        ]
        capabilities = client.capabilities("codex-app-server")
        assert capabilities.persistent_sessions
        assert capabilities.count.max == 4
        assert capabilities.batching.mode == "fan_out"
        assert capabilities.transparent_background == "emulated"
        assert capabilities.batching.native_count.max == 1
        assert capabilities.batching.max_parallel_outputs == 2
        assert capabilities.input_fidelities == ("high",)
        assert capabilities.actions == ("auto",)
        diagnostics = client.diagnostics()
        assert diagnostics.configuration.listener_scope == "loopback"
        assert diagnostics.jobs is not None and diagnostics.jobs.total == 1
        assert diagnostics.providers[1].provider == "codex-responses"
        assert client.session("sdk-fixture").thread_id == "thread_fixture_01"
        client.delete_session("sdk-fixture")
        queued = client.jobs.create(request)
        assert queued.status == "queued"
        assert queued.request.output.response_format == "artifact"
        completed = client.jobs.get(queued.id)
        assert completed.status == "succeeded"
        assert completed.result is not None and completed.result.data[0].type == "artifact"
        assert client.jobs.partial(queued.id).startswith(b"\x89PNG\r\n\x1a\n")
        page = client.jobs.list(
            status="succeeded", visibility="active", favorite=True, search="fixture"
        )
        assert page.items[0].id == queued.id
        assert page.next_cursor == "sdk-next"
        assert client.jobs.update(queued.id, favorite=True, deleted=False).favorite
        assert client.jobs.cancel(queued.id).status == "cancelled"
        preset = client.presets.create(
            ImagePresetCreate(
                name="portrait-high",
                description="Editorial portrait",
                template=ImagePresetTemplate(prompt="Studio portrait"),
            )
        )
        assert preset.name == "portrait-high"
        assert client.presets.get(preset.name).template.operation == "generate"
        assert client.presets.list(limit=5).items[0].name == preset.name
        updated = client.presets.update(
            preset.name,
            ImagePresetWrite(
                description="Updated portrait",
                template=ImagePresetTemplate(prompt="Updated studio portrait"),
            ),
        )
        assert updated.description == "Updated portrait"
        client.presets.delete(preset.name)
        assert client.health()["status"] == "live"


def test_provider_switching_only_changes_request_configuration(
    bridge_url: str, fixture_request: dict[str, object]
) -> None:
    request = ImageRequest.from_dict(fixture_request)
    switched = ImageRequest.from_dict(
        {
            **request.to_dict(),
            "routing": {"provider": "codex-responses", "model": None},
        }
    )
    with ImagegenBridgeClient(bridge_url, bearer_token=TOKEN) as client:
        response = client.images.generate(switched)
    assert response.provider == "codex-responses"


def test_async_client_matches_shared_http_contract(
    bridge_url: str, fixture_request: dict[str, object]
) -> None:
    async def scenario() -> None:
        request = ImageRequest.from_dict(fixture_request)
        async with AsyncImagegenBridgeClient(bridge_url, bearer_token=TOKEN) as client:
            result = await client.images.generate(request)
            assert result.provider == "codex-app-server"
            events = [event async for event in client.images.stream(request)]
            assert [event.type for event in events] == [
                "started",
                "progress",
                "partial_image",
                "completed",
            ]
            providers = await client.providers()
            assert not providers.items[1].experimental
            assert providers.items[1].models == (
                "gpt-image-2",
                "gpt-image-1.5",
                "gpt-image-1",
                "gpt-image-1-mini",
            )
            assert (await client.capabilities("codex-app-server")).generation
            assert (
                await client.capabilities("codex-responses", model="gpt-image-1")
            ).model == "gpt-image-1"
            diagnostics = await client.diagnostics()
            assert diagnostics.artifact_storage_enabled
            assert diagnostics.events is not None
            assert diagnostics.events.items[0].route == "/v1/jobs"
            assert diagnostics.events.items[1].duration_ms == 36
            assert (await client.session("sdk-fixture")).reused
            await client.delete_session("sdk-fixture")
            queued = await client.jobs.create(request)
            assert (await client.jobs.get(queued.id)).result is not None
            assert (await client.jobs.partial(queued.id)).startswith(b"\x89PNG\r\n\x1a\n")
            assert (await client.jobs.list()).items[0].status == "succeeded"
            assert (await client.jobs.update(queued.id, favorite=True, deleted=False)).favorite
            assert (await client.jobs.cancel(queued.id)).cancel_requested
            preset = await client.presets.create(
                ImagePresetCreate(
                    name="portrait-high",
                    template=ImagePresetTemplate(prompt="Studio portrait"),
                )
            )
            assert (await client.presets.get(preset.name)).name == preset.name
            assert (await client.presets.list()).items[0].name == preset.name
            replaced = await client.presets.update(
                preset.name,
                ImagePresetWrite(template=ImagePresetTemplate(prompt="Updated portrait")),
            )
            assert replaced.template.prompt == "Updated portrait"
            await client.presets.delete(preset.name)
            assert (await client.health(ready=True))["status"] == "ready"

    asyncio.run(scenario())


def test_structured_errors_are_available_for_http_and_sse(bridge_url: str) -> None:
    request = ImageRequest.generate("trigger-error")
    with ImagegenBridgeClient(bridge_url, bearer_token=TOKEN) as client:
        with pytest.raises(BridgeAPIError) as raised:
            client.images.generate(request)
        assert raised.value.status_code == 429
        assert raised.value.bridge_code == "rate_limited"
        assert raised.value.retryable
        assert raised.value.request_id == "request_fixture_error"
        assert raised.value.suggestions

        with pytest.raises(BridgeAPIError) as streamed:
            list(client.images.stream(request))
        assert streamed.value.bridge_code == "rate_limited"

        with pytest.raises(ValueError, match="cannot be combined"):
            client.jobs.list(include_deleted=True, visibility="hidden")


def test_bridge_authentication_is_applied(bridge_url: str) -> None:
    with (
        ImagegenBridgeClient(bridge_url, bearer_token="wrong") as client,
        pytest.raises(BridgeAPIError) as raised,
    ):
        client.providers()
    assert raised.value.status_code == 401
    assert raised.value.bridge_code == "invalid_request"
