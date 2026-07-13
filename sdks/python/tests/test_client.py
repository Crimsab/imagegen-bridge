from __future__ import annotations

import asyncio

import pytest

from imagegen_bridge import (
    AsyncImagegenBridgeClient,
    BridgeAPIError,
    CompletedEvent,
    ImagegenBridgeClient,
    ImageRequest,
    PartialImageEvent,
    ProgressEvent,
    StartedEvent,
)

TOKEN = "sdk-test-token"


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
        assert capabilities.input_fidelities == ("high",)
        assert capabilities.actions == ("auto",)
        assert client.session("sdk-fixture").thread_id == "thread_fixture_01"
        client.delete_session("sdk-fixture")
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
            assert (await client.providers()).items[1].experimental
            assert (await client.capabilities("codex-app-server")).generation
            assert (await client.session("sdk-fixture")).reused
            await client.delete_session("sdk-fixture")
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

        with pytest.raises(BridgeAPIError) as streamed:
            list(client.images.stream(request))
        assert streamed.value.bridge_code == "rate_limited"


def test_bridge_authentication_is_applied(bridge_url: str) -> None:
    with (
        ImagegenBridgeClient(bridge_url, bearer_token="wrong") as client,
        pytest.raises(BridgeAPIError) as raised,
    ):
        client.providers()
    assert raised.value.status_code == 401
    assert raised.value.bridge_code == "invalid_request"
