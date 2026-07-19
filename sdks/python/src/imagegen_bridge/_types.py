"""Versioned typed models for the native Imagegen Bridge wire contract."""

from __future__ import annotations

from dataclasses import asdict, dataclass, field
from typing import Any, Literal, TypeAlias, cast

JSONValue: TypeAlias = None | bool | int | float | str | list["JSONValue"] | dict[str, "JSONValue"]
Quality: TypeAlias = Literal["auto", "low", "medium", "high"]
OutputFormat: TypeAlias = Literal["png", "jpeg", "webp"]
Background: TypeAlias = Literal["auto", "opaque", "transparent"]
Moderation: TypeAlias = Literal["auto", "low"]
MultiImageFailurePolicy: TypeAlias = Literal["fail_fast", "best_effort"]
InputFidelity: TypeAlias = Literal["low", "high"]
ImageAction: TypeAlias = Literal["auto", "generate", "edit"]
Resolution: TypeAlias = Literal["1k", "2k", "4k"]
ResponseFormat: TypeAlias = Literal["b64_json", "url", "artifact", "metadata"]
ArtifactCollisionPolicy: TypeAlias = Literal["error", "suffix"]
ArtifactMetadataPolicy: TypeAlias = Literal["none", "sidecar", "embedded", "sidecar_and_embedded"]
CompatibilityMode: TypeAlias = Literal["strict", "normalize", "best_effort"]
NegativePromptMode: TypeAlias = Literal["auto", "native", "merge", "reject"]
RevisedPromptPolicy: TypeAlias = Literal["include", "omit", "require"]
SessionMode: TypeAlias = Literal["isolated", "persistent", "thread"]
SupportLevel: TypeAlias = Literal["unsupported", "emulated", "native"]
BatchMode: TypeAlias = Literal["native", "fan_out"]
TransparencyMode: TypeAlias = Literal["auto", "native", "chroma_key"]
FallbackPolicy: TypeAlias = Literal["on_unavailable", "on_error"]
ProviderAttemptOutcome: TypeAlias = Literal["succeeded", "failed"]
BatchExecution: TypeAlias = Literal["auto", "sequential", "parallel"]
ImageJobStatus: TypeAlias = Literal[
    "queued", "running", "succeeded", "failed", "cancelled", "interrupted"
]
ImageJobVisibility: TypeAlias = Literal["active", "hidden", "all"]
PresetOperation: TypeAlias = Literal["generate", "edit"]


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
    failure_policy: MultiImageFailurePolicy = "fail_fast"
    input_fidelity: InputFidelity | None = None
    action: ImageAction = "auto"

    def to_dict(self) -> dict[str, JSONValue]:
        value = cast(dict[str, JSONValue], asdict(self))
        if self.input_fidelity is None:
            value.pop("input_fidelity")
        return value

    @classmethod
    def from_dict(cls, value: dict[str, Any]) -> GenerationParameters:
        return cls(**value)


@dataclass(frozen=True, slots=True)
class ProviderRoute:
    provider: str
    model: str | None = None

    def to_dict(self) -> dict[str, JSONValue]:
        value: dict[str, JSONValue] = {"provider": self.provider}
        if self.model is not None:
            value["model"] = self.model
        return value


@dataclass(frozen=True, slots=True)
class RoutingOptions:
    provider: str | None = None
    model: str | None = None
    fallbacks: tuple[ProviderRoute, ...] = ()
    fallback_policy: FallbackPolicy = "on_unavailable"

    def to_dict(self) -> dict[str, JSONValue]:
        value: dict[str, JSONValue] = {"provider": self.provider, "model": self.model}
        if self.fallbacks:
            value["fallbacks"] = [_wire(item) for item in self.fallbacks]
            value["fallback_policy"] = self.fallback_policy
        return value

    @classmethod
    def from_dict(cls, value: dict[str, Any]) -> RoutingOptions:
        return cls(
            provider=value.get("provider"),
            model=value.get("model"),
            fallbacks=tuple(ProviderRoute(**item) for item in value.get("fallbacks", [])),
            fallback_policy=value.get("fallback_policy", "on_unavailable"),
        )


@dataclass(frozen=True, slots=True)
class SessionOptions:
    mode: SessionMode = "isolated"
    key: str | None = None
    thread_id: str | None = None


@dataclass(frozen=True, slots=True)
class TransparencyOptions:
    mode: TransparencyMode = "auto"
    key_color: str | None = None
    transparent_threshold: int = 12
    opaque_threshold: int = 96
    despill: bool = True


@dataclass(frozen=True, slots=True)
class OutputOptions:
    response_format: ResponseFormat = "b64_json"
    filename_prefix: str | None = None
    directory: str | None = None
    filename: str | None = None
    collision: ArtifactCollisionPolicy = "error"
    metadata: ArtifactMetadataPolicy = "none"
    transparency: TransparencyOptions = field(default_factory=TransparencyOptions)

    def to_dict(self) -> dict[str, JSONValue]:
        value = cast(dict[str, JSONValue], asdict(self))
        value.pop("transparency")
        if self.transparency != TransparencyOptions():
            value["transparency"] = _wire(asdict(self.transparency))
        return value

    @classmethod
    def from_dict(cls, value: dict[str, Any]) -> OutputOptions:
        fields = dict(value)
        fields["transparency"] = TransparencyOptions(**fields.get("transparency", {}))
        return cls(**fields)


@dataclass(frozen=True, slots=True)
class RequestPolicies:
    compatibility: CompatibilityMode = "strict"
    negative_prompt: NegativePromptMode = "auto"
    revised_prompt: RevisedPromptPolicy = "include"
    batch_execution: BatchExecution = "auto"

    def to_dict(self) -> dict[str, JSONValue]:
        value: dict[str, JSONValue] = {
            "compatibility": self.compatibility,
            "negative_prompt": self.negative_prompt,
            "revised_prompt": self.revised_prompt,
        }
        if self.batch_execution != "auto":
            value["batch_execution"] = self.batch_execution
        return value


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
            "routing": _wire(self.routing),
            "session": _wire(asdict(self.session)),
            "output": _wire(self.output),
            "policies": _wire(self.policies),
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
            routing=RoutingOptions.from_dict(value.get("routing", {})),
            session=SessionOptions(**value.get("session", {})),
            output=OutputOptions.from_dict(value.get("output", {})),
            policies=RequestPolicies(**value.get("policies", {})),
            idempotency_key=value.get("idempotency_key"),
            timeout_ms=value.get("timeout_ms"),
            user=value.get("user"),
        )


@dataclass(frozen=True, slots=True)
class ImagePresetTemplate:
    """Reusable request configuration without image inputs or idempotency state."""

    prompt: str | None = None
    negative_prompt: str | None = None
    operation: PresetOperation = "generate"
    parameters: GenerationParameters = field(default_factory=GenerationParameters)
    routing: RoutingOptions = field(default_factory=RoutingOptions)
    session: SessionOptions = field(default_factory=SessionOptions)
    output: OutputOptions = field(default_factory=OutputOptions)
    policies: RequestPolicies = field(default_factory=RequestPolicies)
    timeout_ms: int | None = None
    user: str | None = None

    def to_dict(self) -> dict[str, JSONValue]:
        value: dict[str, JSONValue] = {
            "operation": self.operation,
            "parameters": _wire(self.parameters),
            "routing": _wire(self.routing),
            "session": _wire(asdict(self.session)),
            "output": _wire(self.output),
            "policies": _wire(self.policies),
        }
        for key, item in (
            ("prompt", self.prompt),
            ("negative_prompt", self.negative_prompt),
            ("timeout_ms", self.timeout_ms),
            ("user", self.user),
        ):
            if item is not None:
                value[key] = item
        return value

    @classmethod
    def from_dict(cls, value: dict[str, Any]) -> ImagePresetTemplate:
        return cls(
            prompt=value.get("prompt"),
            negative_prompt=value.get("negative_prompt"),
            operation=value.get("operation", "generate"),
            parameters=GenerationParameters.from_dict(value.get("parameters", {})),
            routing=RoutingOptions.from_dict(value.get("routing", {})),
            session=SessionOptions(**value.get("session", {})),
            output=OutputOptions.from_dict(value.get("output", {})),
            policies=RequestPolicies(**value.get("policies", {})),
            timeout_ms=value.get("timeout_ms"),
            user=value.get("user"),
        )


@dataclass(frozen=True, slots=True)
class ImagePresetWrite:
    template: ImagePresetTemplate
    description: str | None = None

    def to_dict(self) -> dict[str, JSONValue]:
        value: dict[str, JSONValue] = {"template": _wire(self.template)}
        if self.description is not None:
            value["description"] = self.description
        return value


@dataclass(frozen=True, slots=True)
class ImagePresetCreate:
    name: str
    template: ImagePresetTemplate
    description: str | None = None

    def to_dict(self) -> dict[str, JSONValue]:
        value: dict[str, JSONValue] = {"name": self.name, "template": _wire(self.template)}
        if self.description is not None:
            value["description"] = self.description
        return value


@dataclass(frozen=True, slots=True)
class ImagePreset:
    name: str
    template: ImagePresetTemplate
    created: int
    updated: int
    description: str | None = None

    @classmethod
    def from_dict(cls, value: dict[str, Any]) -> ImagePreset:
        return cls(
            name=value["name"],
            template=ImagePresetTemplate.from_dict(value["template"]),
            created=value["created"],
            updated=value["updated"],
            description=value.get("description"),
        )


@dataclass(frozen=True, slots=True)
class ImagePresetPage:
    items: tuple[ImagePreset, ...]
    next_cursor: str | None = None

    @classmethod
    def from_dict(cls, value: dict[str, Any]) -> ImagePresetPage:
        return cls(
            items=tuple(ImagePreset.from_dict(item) for item in value["items"]),
            next_cursor=value.get("next_cursor"),
        )


@dataclass(frozen=True, slots=True)
class Normalization:
    field: str
    reason: str
    requested: JSONValue = None
    effective: JSONValue = None


@dataclass(frozen=True, slots=True)
class ProviderAttempt:
    provider: str
    outcome: ProviderAttemptOutcome
    duration_ms: int
    model: str | None = None
    error_code: str | None = None


@dataclass(frozen=True, slots=True)
class GeneratedImage:
    type: Literal["b64_json", "url", "artifact", "metadata"]
    index: int
    format: OutputFormat
    width: int
    height: int
    bytes: int
    sha256: str
    generation_ms: int | None = None
    metadata_name: str | None = None
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
class BridgeErrorData:
    code: str
    message: str
    retryable: bool
    provider: str | None = None
    upstream_request_id: str | None = None
    details: dict[str, JSONValue] = field(default_factory=dict)
    suggestions: list[str] = field(default_factory=list)


@dataclass(frozen=True, slots=True)
class ImageFailure:
    index: int
    error: BridgeErrorData
    generation_ms: int

    @classmethod
    def from_dict(cls, value: dict[str, Any]) -> ImageFailure:
        return cls(
            index=value["index"],
            error=BridgeErrorData(**value["error"]),
            generation_ms=value["generation_ms"],
        )


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
    failures: tuple[ImageFailure, ...] = ()
    normalizations: tuple[Normalization, ...] = ()
    attempts: tuple[ProviderAttempt, ...] = ()
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
            failures=tuple(ImageFailure.from_dict(item) for item in value.get("failures", [])),
            normalizations=tuple(Normalization(**item) for item in value.get("normalizations", [])),
            attempts=tuple(ProviderAttempt(**item) for item in value.get("attempts", [])),
            revised_prompt=value.get("revised_prompt"),
            usage=Usage(**value["usage"]) if value.get("usage") is not None else None,
            session=SessionMetadata.from_dict(value["session"])
            if value.get("session") is not None
            else None,
            warnings=tuple(value.get("warnings", [])),
        )


@dataclass(frozen=True, slots=True)
class ImageJobProgress:
    stage: str
    partial_images: int


@dataclass(frozen=True, slots=True)
class ImageJobSummary:
    id: str
    status: ImageJobStatus
    created: int
    updated: int
    favorite: bool
    started: int | None = None
    completed: int | None = None
    progress: ImageJobProgress | None = None
    deleted: int | None = None

    @classmethod
    def from_dict(cls, value: dict[str, Any]) -> ImageJobSummary:
        return cls(
            id=value["id"],
            status=value["status"],
            created=value["created"],
            updated=value["updated"],
            favorite=value["favorite"],
            started=value.get("started"),
            completed=value.get("completed"),
            progress=ImageJobProgress(**value["progress"])
            if value.get("progress") is not None
            else None,
            deleted=value.get("deleted"),
        )


@dataclass(frozen=True, slots=True)
class ImageJob:
    id: str
    status: ImageJobStatus
    created: int
    updated: int
    favorite: bool
    request: ImageRequest
    cancel_requested: bool
    started: int | None = None
    completed: int | None = None
    progress: ImageJobProgress | None = None
    deleted: int | None = None
    result: ImageResponse | None = None
    error: BridgeErrorData | None = None

    @classmethod
    def from_dict(cls, value: dict[str, Any]) -> ImageJob:
        summary = ImageJobSummary.from_dict(value)
        return cls(
            id=summary.id,
            status=summary.status,
            created=summary.created,
            updated=summary.updated,
            favorite=summary.favorite,
            request=ImageRequest.from_dict(value["request"]),
            cancel_requested=value["cancel_requested"],
            started=summary.started,
            completed=summary.completed,
            progress=summary.progress,
            deleted=summary.deleted,
            result=ImageResponse.from_dict(value["result"])
            if value.get("result") is not None
            else None,
            error=BridgeErrorData(**value["error"]) if value.get("error") is not None else None,
        )


@dataclass(frozen=True, slots=True)
class ImageJobPage:
    items: tuple[ImageJobSummary, ...]
    next_cursor: str | None = None

    @classmethod
    def from_dict(cls, value: dict[str, Any]) -> ImageJobPage:
        return cls(
            items=tuple(ImageJobSummary.from_dict(item) for item in value["items"]),
            next_cursor=value.get("next_cursor"),
        )


@dataclass(frozen=True, slots=True)
class ProviderDescriptor:
    name: str
    display_name: str
    version: str
    experimental: bool
    models: tuple[str, ...] = ()

    @classmethod
    def from_dict(cls, value: dict[str, Any]) -> ProviderDescriptor:
        return cls(
            name=value["name"],
            display_name=value["display_name"],
            version=value["version"],
            experimental=value["experimental"],
            models=tuple(value.get("models", ())),
        )


@dataclass(frozen=True, slots=True)
class ProviderPage:
    items: tuple[ProviderDescriptor, ...]
    next_cursor: str | None = None

    @classmethod
    def from_dict(cls, value: dict[str, Any]) -> ProviderPage:
        return cls(
            tuple(ProviderDescriptor.from_dict(item) for item in value["items"]),
            value.get("next_cursor"),
        )


@dataclass(frozen=True, slots=True)
class ConfigurationOrigin:
    field: str
    source: Literal["default", "file", "environment", "override"]
    key: str


@dataclass(frozen=True, slots=True)
class ConfigurationDiagnostics:
    listener_scope: Literal["loopback", "remote", "embedded", "unknown"]
    authentication_required: bool
    metrics_enabled: bool
    jobs_enabled: bool
    # Configured connection cap, or None when unlimited.
    max_connections: int | None
    max_body_bytes: int
    read_timeout_ms: int
    write_timeout_ms: int
    provenance: tuple[ConfigurationOrigin, ...]
    version: int | None = None
    default_provider: str | None = None
    listener_port: int | None = None

    @classmethod
    def from_dict(cls, value: dict[str, Any]) -> ConfigurationDiagnostics:
        copied = dict(value)
        copied["provenance"] = tuple(ConfigurationOrigin(**item) for item in value["provenance"])
        return cls(**copied)


@dataclass(frozen=True, slots=True)
class RuntimeDiagnostics:
    global_queued: int
    providers_queued: dict[str, int]


@dataclass(frozen=True, slots=True)
class JobManagerDiagnostics:
    total: int
    queued: int
    running: int
    succeeded: int
    failed: int
    cancelled: int
    interrupted: int
    hidden: int
    database_bytes: int
    logical_bytes: int
    active_workers: int
    max_pending: int
    max_running: int
    retention_secs: int
    max_retained: int
    max_retained_bytes: int
    max_database_bytes: int


@dataclass(frozen=True, slots=True)
class ProviderReadiness:
    provider: str
    status: Literal["ready", "not_ready"]
    error: BridgeErrorData | None = None

    @classmethod
    def from_dict(cls, value: dict[str, Any]) -> ProviderReadiness:
        return cls(
            provider=value["provider"],
            status=value["status"],
            error=BridgeErrorData(**value["error"]) if value.get("error") is not None else None,
        )


@dataclass(frozen=True, slots=True)
class OperatorEvent:
    sequence: int
    timestamp_ms: int
    method: Literal["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS", "OTHER"]
    route: str
    status: int
    duration_ms: int


@dataclass(frozen=True, slots=True)
class OperatorEventHistory:
    capacity: int
    dropped: int
    items: tuple[OperatorEvent, ...]

    @classmethod
    def from_dict(cls, value: dict[str, Any]) -> OperatorEventHistory:
        return cls(
            capacity=value["capacity"],
            dropped=value["dropped"],
            items=tuple(OperatorEvent(**item) for item in value["items"]),
        )


@dataclass(frozen=True, slots=True)
class OperatorDiagnostics:
    bridge_version: str
    configuration: ConfigurationDiagnostics
    artifact_storage_enabled: bool
    runtime: RuntimeDiagnostics
    providers: tuple[ProviderReadiness, ...]
    jobs: JobManagerDiagnostics | None = None
    events: OperatorEventHistory | None = None

    @classmethod
    def from_dict(cls, value: dict[str, Any]) -> OperatorDiagnostics:
        return cls(
            bridge_version=value["bridge_version"],
            configuration=ConfigurationDiagnostics.from_dict(value["configuration"]),
            artifact_storage_enabled=value["artifact_storage_enabled"],
            runtime=RuntimeDiagnostics(**value["runtime"]),
            providers=tuple(ProviderReadiness.from_dict(item) for item in value["providers"]),
            jobs=JobManagerDiagnostics(**value["jobs"]) if value.get("jobs") is not None else None,
            events=(
                OperatorEventHistory.from_dict(value["events"])
                if value.get("events") is not None
                else None
            ),
        )


@dataclass(frozen=True, slots=True)
class U8Range:
    min: int
    max: int


@dataclass(frozen=True, slots=True)
class BatchCapabilities:
    mode: BatchMode
    native_count: U8Range
    max_parallel_outputs: int


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
    batching: BatchCapabilities
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
    input_fidelities: tuple[InputFidelity, ...]
    actions: tuple[ImageAction, ...]
    reference_images: InputCapabilities
    edit_images: InputCapabilities
    masks: InputCapabilities
    partial_images: U8Range
    persistent_sessions: bool
    explicit_threads: bool
    transparent_background: SupportLevel = "unsupported"

    @classmethod
    def from_dict(cls, value: dict[str, Any]) -> ProviderCapabilities:
        copied = dict(value)
        for key in (
            "qualities",
            "output_formats",
            "backgrounds",
            "moderation",
            "input_fidelities",
            "actions",
        ):
            copied[key] = tuple(copied[key])
        copied["count"] = U8Range(**copied["count"])
        copied["batching"] = BatchCapabilities(
            mode=copied["batching"]["mode"],
            native_count=U8Range(**copied["batching"]["native_count"]),
            max_parallel_outputs=copied["batching"]["max_parallel_outputs"],
        )
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
