"""Versioned typed models for the native Imagegen Bridge wire contract."""

from __future__ import annotations

from dataclasses import asdict, dataclass, field
from typing import Any, Literal, TypeAlias, cast

JSONValue: TypeAlias = None | bool | int | float | str | list["JSONValue"] | dict[str, "JSONValue"]
Quality: TypeAlias = Literal["auto", "low", "medium", "high"]
OutputFormat: TypeAlias = Literal["png", "jpeg", "webp"]
Background: TypeAlias = Literal["auto", "opaque", "transparent"]
Moderation: TypeAlias = Literal["auto", "low"]
Resolution: TypeAlias = Literal["1k", "2k", "4k"]
ResponseFormat: TypeAlias = Literal["b64_json", "url", "artifact", "metadata"]
CompatibilityMode: TypeAlias = Literal["strict", "normalize", "best_effort"]
NegativePromptMode: TypeAlias = Literal["auto", "native", "merge", "reject"]
RevisedPromptPolicy: TypeAlias = Literal["include", "omit", "require"]
SessionMode: TypeAlias = Literal["isolated", "persistent", "thread"]
SupportLevel: TypeAlias = Literal["unsupported", "emulated", "native"]


def _wire(value: Any) -> JSONValue:
    if hasattr(value, "to_dict"):
        return cast(JSONValue, value.to_dict())
    if isinstance(value, tuple):
        return [_wire(item) for item in value]
    if isinstance(value, list):
        return [_wire(item) for item in value]
    if isinstance(value, dict):
        return {str(key): _wire(item) for key, item in value.items()}
    return cast(JSONValue, value)


@dataclass(frozen=True, slots=True)
class ImageInput:
    """One bounded input image using exactly one source representation."""

    type: Literal["file", "url", "data_url", "base64"]
    value: str
    media_type: str | None = None
    filename: str | None = None

    @classmethod
    def file(
        cls, path: str, *, media_type: str | None = None, filename: str | None = None
    ) -> ImageInput:
        return cls("file", path, media_type, filename)

    @classmethod
    def url(
        cls, url: str, *, media_type: str | None = None, filename: str | None = None
    ) -> ImageInput:
        return cls("url", url, media_type, filename)

    @classmethod
    def data_url(cls, data_url: str, *, filename: str | None = None) -> ImageInput:
        return cls("data_url", data_url, None, filename)

    @classmethod
    def base64(
        cls, data: str, *, media_type: str | None = None, filename: str | None = None
    ) -> ImageInput:
        return cls("base64", data, media_type, filename)

    def to_dict(self) -> dict[str, JSONValue]:
        key = {"file": "path", "url": "url", "data_url": "data_url", "base64": "data"}[self.type]
        value: dict[str, JSONValue] = {"type": self.type, key: self.value}
        if self.media_type is not None:
            value["media_type"] = self.media_type
        if self.filename is not None:
            value["filename"] = self.filename
        return value

    @classmethod
    def from_dict(cls, value: dict[str, Any]) -> ImageInput:
        kind = cast(Literal["file", "url", "data_url", "base64"], value["type"])
        key = {"file": "path", "url": "url", "data_url": "data_url", "base64": "data"}[kind]
        return cls(kind, str(value[key]), value.get("media_type"), value.get("filename"))


@dataclass(frozen=True, slots=True)
class GenerationParameters:
    n: int = 1
    size: str = "auto"
    aspect_ratio: str | None = None
    resolution: Resolution | None = None
    quality: Quality = "auto"
    output_format: OutputFormat = "png"
    output_compression: int | None = None
    background: Background = "auto"
    moderation: Moderation = "auto"
    partial_images: int = 0

    def to_dict(self) -> dict[str, JSONValue]:
        return cast(dict[str, JSONValue], asdict(self))

    @classmethod
    def from_dict(cls, value: dict[str, Any]) -> GenerationParameters:
        return cls(**value)


@dataclass(frozen=True, slots=True)
class RoutingOptions:
    provider: str | None = None
    model: str | None = None


@dataclass(frozen=True, slots=True)
class SessionOptions:
    mode: SessionMode = "isolated"
    key: str | None = None
    thread_id: str | None = None


@dataclass(frozen=True, slots=True)
class OutputOptions:
    response_format: ResponseFormat = "b64_json"
    filename_prefix: str | None = None


@dataclass(frozen=True, slots=True)
class RequestPolicies:
    compatibility: CompatibilityMode = "strict"
    negative_prompt: NegativePromptMode = "auto"
    revised_prompt: RevisedPromptPolicy = "include"


@dataclass(frozen=True, slots=True)
class ImageRequest:
    prompt: str
    operation: Literal["generate", "edit"] = "generate"
    negative_prompt: str | None = None
    reference_images: tuple[ImageInput, ...] = ()
    images: tuple[ImageInput, ...] = ()
    mask: ImageInput | None = None
    parameters: GenerationParameters = field(default_factory=GenerationParameters)
    routing: RoutingOptions = field(default_factory=RoutingOptions)
    session: SessionOptions = field(default_factory=SessionOptions)
    output: OutputOptions = field(default_factory=OutputOptions)
    policies: RequestPolicies = field(default_factory=RequestPolicies)
    idempotency_key: str | None = None
    timeout_ms: int | None = None
    user: str | None = None
    version: str = "1"

    @classmethod
    def generate(cls, prompt: str, **kwargs: Any) -> ImageRequest:
        return cls(prompt=prompt, operation="generate", **kwargs)

    @classmethod
    def edit(
        cls,
        prompt: str,
        images: tuple[ImageInput, ...],
        **kwargs: Any,
    ) -> ImageRequest:
        return cls(prompt=prompt, operation="edit", images=images, **kwargs)

    def to_dict(self) -> dict[str, JSONValue]:
        value: dict[str, JSONValue] = {
            "version": self.version,
            "prompt": self.prompt,
            "negative_prompt": self.negative_prompt,
            "operation": self.operation,
            "reference_images": [_wire(item) for item in self.reference_images],
            "parameters": _wire(self.parameters),
            "routing": _wire(asdict(self.routing)),
            "session": _wire(asdict(self.session)),
            "output": _wire(asdict(self.output)),
            "policies": _wire(asdict(self.policies)),
            "idempotency_key": self.idempotency_key,
            "timeout_ms": self.timeout_ms,
            "user": self.user,
        }
        if self.operation == "edit":
            value["images"] = [_wire(item) for item in self.images]
            value["mask"] = _wire(self.mask)
        return value

    @classmethod
    def from_dict(cls, value: dict[str, Any]) -> ImageRequest:
        return cls(
            version=value.get("version", "1"),
            prompt=value["prompt"],
            negative_prompt=value.get("negative_prompt"),
            operation=value.get("operation", "generate"),
            reference_images=tuple(
                ImageInput.from_dict(item) for item in value.get("reference_images", [])
            ),
            images=tuple(ImageInput.from_dict(item) for item in value.get("images", [])),
            mask=ImageInput.from_dict(value["mask"]) if value.get("mask") else None,
            parameters=GenerationParameters.from_dict(value.get("parameters", {})),
            routing=RoutingOptions(**value.get("routing", {})),
            session=SessionOptions(**value.get("session", {})),
            output=OutputOptions(**value.get("output", {})),
            policies=RequestPolicies(**value.get("policies", {})),
            idempotency_key=value.get("idempotency_key"),
            timeout_ms=value.get("timeout_ms"),
            user=value.get("user"),
        )


@dataclass(frozen=True, slots=True)
class Normalization:
    field: str
    reason: str
    requested: JSONValue = None
    effective: JSONValue = None


@dataclass(frozen=True, slots=True)
class GeneratedImage:
    type: Literal["b64_json", "url", "artifact", "metadata"]
    format: OutputFormat
    width: int
    height: int
    bytes: int
    sha256: str
    b64_json: str | None = None
    url: str | None = None
    id: str | None = None
    name: str | None = None

    @classmethod
    def from_dict(cls, value: dict[str, Any]) -> GeneratedImage:
        return cls(**value)


@dataclass(frozen=True, slots=True)
class Usage:
    input_tokens: int | None = None
    output_tokens: int | None = None
    total_tokens: int | None = None
    provider: dict[str, int] = field(default_factory=dict)


@dataclass(frozen=True, slots=True)
class SessionMetadata:
    reused: bool
    key: str | None = None
    thread_id: str | None = None

    @classmethod
    def from_dict(cls, value: dict[str, Any]) -> SessionMetadata:
        return cls(**value)


@dataclass(frozen=True, slots=True)
class Timings:
    queue_ms: int = 0
    input_ms: int = 0
    provider_ms: int = 0
    artifact_ms: int = 0
    total_ms: int = 0


@dataclass(frozen=True, slots=True)
class ImageResponse:
    id: str
    created: int
    provider: str
    model: str
    requested: GenerationParameters
    effective: GenerationParameters
    data: tuple[GeneratedImage, ...]
    timings: Timings
    normalizations: tuple[Normalization, ...] = ()
    revised_prompt: str | None = None
    usage: Usage | None = None
    session: SessionMetadata | None = None
    warnings: tuple[str, ...] = ()

    @classmethod
    def from_dict(cls, value: dict[str, Any]) -> ImageResponse:
        return cls(
            id=value["id"],
            created=value["created"],
            provider=value["provider"],
            model=value["model"],
            requested=GenerationParameters.from_dict(value["requested"]),
            effective=GenerationParameters.from_dict(value["effective"]),
            data=tuple(GeneratedImage.from_dict(item) for item in value["data"]),
            timings=Timings(**value["timings"]),
            normalizations=tuple(Normalization(**item) for item in value.get("normalizations", [])),
            revised_prompt=value.get("revised_prompt"),
            usage=Usage(**value["usage"]) if value.get("usage") is not None else None,
            session=SessionMetadata.from_dict(value["session"])
            if value.get("session") is not None
            else None,
            warnings=tuple(value.get("warnings", [])),
        )


@dataclass(frozen=True, slots=True)
class ProviderDescriptor:
    name: str
    display_name: str
    version: str
    experimental: bool


@dataclass(frozen=True, slots=True)
class ProviderPage:
    items: tuple[ProviderDescriptor, ...]
    next_cursor: str | None = None

    @classmethod
    def from_dict(cls, value: dict[str, Any]) -> ProviderPage:
        return cls(
            tuple(ProviderDescriptor(**item) for item in value["items"]), value.get("next_cursor")
        )


@dataclass(frozen=True, slots=True)
class U8Range:
    min: int
    max: int


@dataclass(frozen=True, slots=True)
class SizeCapabilities:
    auto: bool
    allowed: tuple[str, ...]
    arbitrary: bool
    min_edge: int | None = None
    max_edge: int | None = None
    edge_multiple: int | None = None
    min_pixels: int | None = None
    max_pixels: int | None = None
    max_aspect_ratio: float | None = None


@dataclass(frozen=True, slots=True)
class InputCapabilities:
    support: SupportLevel
    max_count: int
    max_bytes_each: int
    max_bytes_total: int


@dataclass(frozen=True, slots=True)
class ProviderCapabilities:
    provider: str
    model: str | None
    implementation_version: str
    experimental: bool
    generation: bool
    edits: bool
    count: U8Range
    sizes: SizeCapabilities
    aspect_ratio: SupportLevel
    resolution: SupportLevel
    qualities: tuple[Quality, ...]
    output_formats: tuple[OutputFormat, ...]
    backgrounds: tuple[Background, ...]
    moderation: tuple[Moderation, ...]
    negative_prompt: SupportLevel
    revised_prompt: SupportLevel
    user_attribution: SupportLevel
    reference_images: InputCapabilities
    edit_images: InputCapabilities
    masks: InputCapabilities
    partial_images: U8Range
    persistent_sessions: bool
    explicit_threads: bool

    @classmethod
    def from_dict(cls, value: dict[str, Any]) -> ProviderCapabilities:
        copied = dict(value)
        for key in ("qualities", "output_formats", "backgrounds", "moderation"):
            copied[key] = tuple(copied[key])
        copied["count"] = U8Range(**copied["count"])
        copied["sizes"] = SizeCapabilities(
            allowed=tuple(copied["sizes"]["allowed"]),
            **{k: v for k, v in copied["sizes"].items() if k != "allowed"},
        )
        for key in ("reference_images", "edit_images", "masks"):
            copied[key] = InputCapabilities(**copied[key])
        copied["partial_images"] = U8Range(**copied["partial_images"])
        return cls(**copied)


@dataclass(frozen=True, slots=True)
class StartedEvent:
    type: Literal["started"] = "started"


@dataclass(frozen=True, slots=True)
class ProgressEvent:
    stage: str
    type: Literal["progress"] = "progress"


@dataclass(frozen=True, slots=True)
class PartialImageEvent:
    index: int
    partial_index: int
    b64_json: str
    type: Literal["partial_image"] = "partial_image"


@dataclass(frozen=True, slots=True)
class CompletedEvent:
    response: ImageResponse
    type: Literal["completed"] = "completed"


StreamEvent: TypeAlias = StartedEvent | ProgressEvent | PartialImageEvent | CompletedEvent
