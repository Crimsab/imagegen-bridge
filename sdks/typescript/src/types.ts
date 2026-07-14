export type JsonValue =
  | null
  | boolean
  | number
  | string
  | JsonValue[]
  | { [key: string]: JsonValue };
export type Quality = "auto" | "low" | "medium" | "high";
export type OutputFormat = "png" | "jpeg" | "webp";
export type Background = "auto" | "opaque" | "transparent";
export type Moderation = "auto" | "low";
export type MultiImageFailurePolicy = "fail_fast" | "best_effort";
export type InputFidelity = "low" | "high";
export type ImageAction = "auto" | "generate" | "edit";
export type Resolution = "1k" | "2k" | "4k";
export type ResponseFormat = "b64_json" | "url" | "artifact" | "metadata";
export type ArtifactCollisionPolicy = "error" | "suffix";
export type ArtifactMetadataPolicy = "none" | "sidecar" | "embedded" | "sidecar_and_embedded";
export type CompatibilityMode = "strict" | "normalize" | "best_effort";
export type NegativePromptMode = "auto" | "native" | "merge" | "reject";
export type RevisedPromptPolicy = "include" | "omit" | "require";
export type SessionMode = "isolated" | "persistent" | "thread";
export type SupportLevel = "unsupported" | "emulated" | "native";

interface InputMetadata {
  media_type?: string | null;
  filename?: string | null;
}

export type ImageInput =
  | ({ type: "file"; path: string } & InputMetadata)
  | ({ type: "url"; url: string } & InputMetadata)
  | ({ type: "data_url"; data_url: string } & InputMetadata)
  | ({ type: "base64"; data: string } & InputMetadata);

export interface GenerationParameters {
  n?: number;
  size?: "auto" | `${number}x${number}`;
  aspect_ratio?: string | null;
  resolution?: Resolution | null;
  quality?: Quality;
  output_format?: OutputFormat;
  output_compression?: number | null;
  background?: Background;
  moderation?: Moderation;
  partial_images?: number;
  failure_policy?: MultiImageFailurePolicy;
  input_fidelity?: InputFidelity | null;
  action?: ImageAction;
}

export interface RoutingOptions {
  provider?: string | null;
  model?: string | null;
}

export interface SessionOptions {
  mode?: SessionMode;
  key?: string | null;
  thread_id?: string | null;
}

export interface OutputOptions {
  response_format?: ResponseFormat;
  filename_prefix?: string | null;
  directory?: string | null;
  filename?: string | null;
  collision?: ArtifactCollisionPolicy;
  metadata?: ArtifactMetadataPolicy;
}

export interface RequestPolicies {
  compatibility?: CompatibilityMode;
  negative_prompt?: NegativePromptMode;
  revised_prompt?: RevisedPromptPolicy;
}

interface ImageRequestBase {
  version?: "1";
  prompt: string;
  negative_prompt?: string | null;
  parameters?: GenerationParameters;
  routing?: RoutingOptions;
  session?: SessionOptions;
  output?: OutputOptions;
  policies?: RequestPolicies;
  idempotency_key?: string | null;
  timeout_ms?: number | null;
  user?: string | null;
}

export interface GenerateImageRequest extends ImageRequestBase {
  operation: "generate";
  reference_images?: ImageInput[];
}

export interface EditImageRequest extends ImageRequestBase {
  operation: "edit";
  images: ImageInput[];
  mask?: ImageInput | null;
  reference_images?: ImageInput[];
}

export type ImageRequest = GenerateImageRequest | EditImageRequest;

export interface Normalization {
  field: string;
  reason: string;
  requested?: JsonValue;
  effective?: JsonValue;
}

interface GeneratedImageBase {
  index: number;
  format: OutputFormat;
  width: number;
  height: number;
  bytes: number;
  sha256: string;
  generation_ms?: number | null;
  metadata_name?: string | null;
}

export type GeneratedImage =
  | (GeneratedImageBase & { type: "b64_json"; b64_json: string })
  | (GeneratedImageBase & { type: "url"; url: string })
  | (GeneratedImageBase & { type: "artifact"; id: string; name?: string | null })
  | (GeneratedImageBase & { type: "metadata" });

export interface Usage {
  input_tokens?: number | null;
  output_tokens?: number | null;
  total_tokens?: number | null;
  provider?: Record<string, number>;
}

export interface SessionMetadata {
  key?: string | null;
  thread_id?: string | null;
  reused: boolean;
}

export interface Timings {
  queue_ms: number;
  input_ms: number;
  provider_ms: number;
  artifact_ms: number;
  total_ms: number;
}

export interface ImageResponse {
  id: string;
  created: number;
  provider: string;
  model: string;
  requested: Required<
    Pick<
      GenerationParameters,
      | "n"
      | "size"
      | "quality"
      | "output_format"
      | "background"
      | "moderation"
      | "partial_images"
      | "failure_policy"
      | "action"
    >
  > &
    GenerationParameters;
  effective: Required<
    Pick<
      GenerationParameters,
      | "n"
      | "size"
      | "quality"
      | "output_format"
      | "background"
      | "moderation"
      | "partial_images"
      | "failure_policy"
      | "action"
    >
  > &
    GenerationParameters;
  normalizations?: Normalization[];
  data: GeneratedImage[];
  failures?: ImageFailure[];
  revised_prompt?: string | null;
  usage?: Usage | null;
  session?: SessionMetadata | null;
  timings: Timings;
  warnings?: string[];
}

export interface BridgeErrorData {
  code: string;
  message: string;
  retryable: boolean;
  provider?: string | null;
  upstream_request_id?: string | null;
  details?: Record<string, JsonValue>;
}

export interface ImageFailure {
  index: number;
  error: BridgeErrorData;
  generation_ms: number;
}

export type ImageJobStatus =
  | "queued"
  | "running"
  | "succeeded"
  | "failed"
  | "cancelled"
  | "interrupted";

export interface ImageJobProgress {
  stage: string;
  partial_images: number;
}

export interface ImageJobSummary {
  id: string;
  status: ImageJobStatus;
  created: number;
  updated: number;
  started?: number | null;
  completed?: number | null;
  progress?: ImageJobProgress | null;
  favorite: boolean;
  deleted?: number | null;
}

export interface ImageJob extends ImageJobSummary {
  request: ImageRequest;
  result?: ImageResponse | null;
  error?: BridgeErrorData | null;
  cancel_requested: boolean;
}

export interface ImageJobPage {
  items: ImageJobSummary[];
  next_cursor?: string | null;
}

export interface ImageJobUpdate {
  favorite?: boolean;
  deleted?: boolean;
}

export interface ProviderDescriptor {
  name: string;
  display_name: string;
  version: string;
  experimental: boolean;
  models?: string[];
}

export interface ProviderPage {
  items: ProviderDescriptor[];
  next_cursor?: string | null;
}

export interface ConfigurationOrigin {
  field: string;
  source: "default" | "file" | "environment" | "override";
  key: string;
}

export interface ConfigurationDiagnostics {
  version?: number | null;
  default_provider?: string | null;
  listener_scope: "loopback" | "remote" | "embedded" | "unknown";
  listener_port?: number | null;
  authentication_required: boolean;
  metrics_enabled: boolean;
  jobs_enabled: boolean;
  max_connections: number;
  max_body_bytes: number;
  read_timeout_ms: number;
  write_timeout_ms: number;
  provenance: ConfigurationOrigin[];
}

export interface RuntimeDiagnostics {
  global_queued: number;
  providers_queued: Record<string, number>;
}

export interface JobManagerDiagnostics {
  total: number;
  queued: number;
  running: number;
  succeeded: number;
  failed: number;
  cancelled: number;
  interrupted: number;
  hidden: number;
  database_bytes: number;
  active_workers: number;
  max_pending: number;
  max_running: number;
  retention_secs: number;
  max_retained: number;
}

export interface ProviderReadiness {
  provider: string;
  status: "ready" | "not_ready";
  error?: BridgeErrorData | null;
}

export interface OperatorEvent {
  sequence: number;
  timestamp_ms: number;
  method: "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS" | "OTHER";
  route: string;
  status: number;
  duration_ms: number;
}

export interface OperatorEventHistory {
  capacity: number;
  dropped: number;
  items: OperatorEvent[];
}

export interface OperatorDiagnostics {
  bridge_version: string;
  configuration: ConfigurationDiagnostics;
  artifact_storage_enabled: boolean;
  runtime: RuntimeDiagnostics;
  jobs?: JobManagerDiagnostics | null;
  providers: ProviderReadiness[];
  events?: OperatorEventHistory | null;
}

export interface U8Range {
  min: number;
  max: number;
}
export interface SizeCapabilities {
  auto: boolean;
  allowed?: string[];
  arbitrary: boolean;
  min_edge?: number | null;
  max_edge?: number | null;
  edge_multiple?: number | null;
  min_pixels?: number | null;
  max_pixels?: number | null;
  max_aspect_ratio?: number | null;
}
export interface InputCapabilities {
  support: SupportLevel;
  max_count: number;
  max_bytes_each: number;
  max_bytes_total: number;
}
export interface ProviderCapabilities {
  provider: string;
  model?: string | null;
  implementation_version: string;
  experimental: boolean;
  generation: boolean;
  edits: boolean;
  count: U8Range;
  sizes: SizeCapabilities;
  aspect_ratio: SupportLevel;
  resolution: SupportLevel;
  qualities: Quality[];
  output_formats: OutputFormat[];
  backgrounds: Background[];
  moderation: Moderation[];
  negative_prompt: SupportLevel;
  revised_prompt: SupportLevel;
  user_attribution: SupportLevel;
  input_fidelities: InputFidelity[];
  actions: ImageAction[];
  reference_images: InputCapabilities;
  edit_images: InputCapabilities;
  masks: InputCapabilities;
  partial_images: U8Range;
  persistent_sessions: boolean;
  explicit_threads: boolean;
}

export type StreamEvent =
  | { type: "started" }
  | { type: "progress"; stage: string }
  | { type: "partial_image"; index: number; partial_index: number; b64_json: string }
  | { type: "completed"; response: ImageResponse };

export type ImageJobVisibility = "active" | "hidden" | "all";

export interface RequestOptions {
  idempotencyKey?: string;
  signal?: AbortSignal;
  timeoutMs?: number;
}

export interface JobListOptions extends RequestOptions {
  limit?: number;
  cursor?: string;
  status?: ImageJobStatus;
  visibility?: ImageJobVisibility;
  favorite?: boolean;
  search?: string;
  /** @deprecated Prefer `visibility: "all"`. */
  includeDeleted?: boolean;
}
