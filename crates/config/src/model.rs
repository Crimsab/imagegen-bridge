//! Serializable configuration document with secret-reference-only auth fields.

use std::{collections::BTreeMap, path::PathBuf};

use serde::{Deserialize, Serialize};

/// Current configuration document version.
pub const CONFIG_VERSION: u32 = 1;

/// Complete application configuration shared by library bootstrap, CLI, and server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BridgeConfig {
    /// Configuration schema version.
    pub version: u32,
    /// Provider selected when a request does not specify one.
    pub default_provider: String,
    /// Shared orchestration limits.
    pub runtime: RuntimeSettings,
    /// Input loading policy.
    pub inputs: InputSettings,
    /// Output storage and retention policy.
    pub artifacts: ArtifactSettings,
    /// Provider-specific bootstrap settings.
    pub providers: ProviderSettings,
    /// HTTP service settings.
    pub server: ServerSettings,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            default_provider: "codex-responses".to_owned(),
            runtime: RuntimeSettings::default(),
            inputs: InputSettings::default(),
            artifacts: ArtifactSettings::default(),
            providers: ProviderSettings::default(),
            server: ServerSettings::default(),
        }
    }
}

/// Runtime timeout, admission, request, and idempotency settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RuntimeSettings {
    /// Default request deadline in milliseconds.
    pub default_timeout_ms: u64,
    /// Cooperative cancellation cleanup window in milliseconds.
    pub cancellation_grace_ms: u64,
    /// Graceful shutdown drain window in milliseconds.
    pub shutdown_grace_ms: u64,
    /// Global concurrency and queue bound.
    pub global: ConcurrencySettings,
    /// Default per-provider concurrency and queue bound.
    pub provider_default: ConcurrencySettings,
    /// Per-provider overrides by stable provider name.
    pub providers: BTreeMap<String, ConcurrencySettings>,
    /// Default per-provider circuit-breaker policy.
    pub circuit_breaker: CircuitBreakerSettings,
    /// Per-provider circuit-breaker overrides by stable provider name.
    pub circuit_breakers: BTreeMap<String, CircuitBreakerSettings>,
    /// Intrinsic request limits.
    pub request: RequestLimitSettings,
    /// Idempotency replay bounds.
    pub idempotency: IdempotencySettings,
}

impl Default for RuntimeSettings {
    fn default() -> Self {
        Self {
            default_timeout_ms: 5 * 60 * 1_000,
            cancellation_grace_ms: 1_000,
            shutdown_grace_ms: 10_000,
            global: ConcurrencySettings {
                max_concurrent: 16,
                max_queued: 64,
            },
            provider_default: ConcurrencySettings::default(),
            providers: BTreeMap::new(),
            circuit_breaker: CircuitBreakerSettings::default(),
            circuit_breakers: BTreeMap::new(),
            request: RequestLimitSettings::default(),
            idempotency: IdempotencySettings::default(),
        }
    }
}

/// One provider circuit-breaker policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CircuitBreakerSettings {
    /// Enable the breaker for this provider.
    pub enabled: bool,
    /// Consecutive dependency failures required to open the circuit.
    pub failure_threshold: u32,
    /// Recovery delay before the first half-open probe, in milliseconds.
    pub open_duration_ms: u64,
    /// Simultaneous half-open probes.
    pub half_open_max_calls: u32,
    /// Successful half-open probes required to close the circuit.
    pub success_threshold: u32,
}

impl Default for CircuitBreakerSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            failure_threshold: 5,
            open_duration_ms: 3 * 60 * 1_000,
            half_open_max_calls: 1,
            success_threshold: 1,
        }
    }
}

/// One fixed-capacity execution pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConcurrencySettings {
    /// Maximum executing calls.
    pub max_concurrent: usize,
    /// Maximum waiting calls.
    pub max_queued: usize,
}

impl Default for ConcurrencySettings {
    fn default() -> Self {
        Self {
            max_concurrent: 4,
            max_queued: 16,
        }
    }
}

/// Intrinsic request bounds applied before provider work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RequestLimitSettings {
    /// Maximum positive prompt bytes.
    pub max_prompt_bytes: usize,
    /// Maximum negative prompt bytes.
    pub max_negative_prompt_bytes: usize,
    /// Maximum output count before provider negotiation.
    pub max_outputs: u8,
    /// Maximum aggregate input image count.
    pub max_inputs: usize,
    /// Maximum inline encoded input characters.
    pub max_inline_encoded_bytes: usize,
    /// Maximum generic explicit image edge.
    pub max_edge: u32,
    /// Maximum request timeout in milliseconds.
    pub max_timeout_ms: u64,
    /// Maximum caller identifier bytes.
    pub max_identifier_bytes: usize,
}

impl Default for RequestLimitSettings {
    fn default() -> Self {
        let limits = imagegen_bridge_core::RequestLimits::default();
        Self {
            max_prompt_bytes: limits.max_prompt_bytes,
            max_negative_prompt_bytes: limits.max_negative_prompt_bytes,
            max_outputs: limits.max_outputs,
            max_inputs: limits.max_inputs,
            max_inline_encoded_bytes: limits.max_inline_encoded_bytes,
            max_edge: limits.max_edge,
            max_timeout_ms: limits.max_timeout_ms,
            max_identifier_bytes: limits.max_identifier_bytes,
        }
    }
}

/// In-memory idempotency coordination and replay retention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IdempotencySettings {
    /// Maximum retained keys.
    pub max_entries: usize,
    /// Maximum serialized bytes retained across completed responses.
    pub max_completed_bytes: usize,
    /// Completed replay lifetime in seconds.
    pub completed_ttl_secs: u64,
    /// Maximum abandoned leader lifetime in seconds.
    pub in_flight_ttl_secs: u64,
    /// Retention for provider operations with unknown outcomes.
    pub unknown_ttl_secs: u64,
}

impl Default for IdempotencySettings {
    fn default() -> Self {
        Self {
            max_entries: 10_000,
            max_completed_bytes: 256 * 1024 * 1024,
            completed_ttl_secs: 24 * 60 * 60,
            in_flight_ttl_secs: 31 * 60,
            unknown_ttl_secs: 24 * 60 * 60,
        }
    }
}

/// Local and remote input policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InputSettings {
    /// Capability roots allowed for local file inputs.
    pub local_roots: Vec<PathBuf>,
    /// Remote reference image fetch policy.
    pub remote: RemoteImageSettings,
}

impl Default for InputSettings {
    fn default() -> Self {
        Self {
            local_roots: vec![PathBuf::from(".")],
            remote: RemoteImageSettings::default(),
        }
    }
}

/// SSRF and size policy for remote image retrieval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RemoteImageSettings {
    /// Whether remote URLs are accepted.
    pub enabled: bool,
    /// Exact lowercase allowed hosts; empty means any public host.
    pub allowed_hosts: Vec<String>,
    /// Allowed destination ports.
    pub allowed_ports: Vec<u16>,
    /// Explicitly allow private/reserved networks.
    pub allow_private_networks: bool,
    /// Maximum redirect hops.
    pub max_redirects: u8,
    /// Per-hop timeout in milliseconds.
    pub timeout_ms: u64,
    /// Maximum URL bytes.
    pub max_url_bytes: usize,
}

impl Default for RemoteImageSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            allowed_hosts: Vec::new(),
            allowed_ports: vec![80, 443],
            allow_private_networks: false,
            max_redirects: 3,
            timeout_ms: 20_000,
            max_url_bytes: 8 * 1024,
        }
    }
}

/// Artifact verification, storage, public delivery, and retention settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ArtifactSettings {
    /// Bridge-owned storage root.
    pub root: PathBuf,
    /// Optional credential-free HTTP(S) base ending in `/`.
    pub public_base_url: Option<String>,
    /// Maximum provider base64 characters.
    pub max_base64_chars: usize,
    /// Maximum aggregate response bytes held across materialization stages.
    pub max_response_bytes: usize,
    /// Encoded and decoded image limits.
    pub image: ImageLimitSettings,
    /// Provider-hosted output URL retrieval policy.
    pub remote_output: RemoteImageSettings,
    /// Cleanup policy.
    pub retention: RetentionSettings,
}

impl Default for ArtifactSettings {
    fn default() -> Self {
        Self {
            root: PathBuf::from("./data/artifacts"),
            public_base_url: None,
            max_base64_chars: 128 * 1024 * 1024,
            max_response_bytes: 256 * 1024 * 1024,
            image: ImageLimitSettings::default(),
            remote_output: RemoteImageSettings::default(),
            retention: RetentionSettings::default(),
        }
    }
}

/// Encoded image and decoder allocation bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ImageLimitSettings {
    /// Maximum encoded bytes.
    pub max_encoded_bytes: u64,
    /// Maximum width or height.
    pub max_edge: u32,
    /// Maximum decoded pixels.
    pub max_pixels: u64,
    /// Maximum decoder allocation bytes.
    pub max_decode_alloc: u64,
}

impl Default for ImageLimitSettings {
    fn default() -> Self {
        let limits = imagegen_bridge_artifacts::ImageLimits::default();
        Self {
            max_encoded_bytes: limits.max_encoded_bytes,
            max_edge: limits.max_edge,
            max_pixels: limits.max_pixels,
            max_decode_alloc: limits.max_decode_alloc,
        }
    }
}

/// Ownership-safe artifact cleanup settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetentionSettings {
    /// Maximum artifact age in seconds.
    pub max_age_secs: u64,
    /// Optional newest artifact count to retain.
    pub max_artifacts: Option<usize>,
    /// Maximum ownership records inspected per pass.
    pub max_scan_entries: usize,
}

impl Default for RetentionSettings {
    fn default() -> Self {
        Self {
            max_age_secs: 7 * 24 * 60 * 60,
            max_artifacts: None,
            max_scan_entries: 100_000,
        }
    }
}

/// All provider bootstrap settings; credentials are referenced by environment name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderSettings {
    /// Stable app-server provider.
    pub codex_app_server: CodexAppServerSettings,
    /// Codex OAuth Responses adapter.
    pub codex_responses: CodexResponsesSettings,
    /// Official `OpenAI` Images adapter.
    pub openai: OpenAiSettings,
}

impl Default for ProviderSettings {
    fn default() -> Self {
        Self {
            codex_app_server: CodexAppServerSettings::default(),
            codex_responses: CodexResponsesSettings::default(),
            openai: OpenAiSettings::default(),
        }
    }
}

/// Owned Codex app-server process and session database settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CodexAppServerSettings {
    /// Register this provider.
    pub enabled: bool,
    /// Codex executable path or command name.
    pub executable: PathBuf,
    /// Extra arguments after `app-server`.
    pub args: Vec<String>,
    /// Optional provider working directory.
    pub cwd: Option<PathBuf>,
    /// Optional Codex orchestration model.
    pub codex_model: Option<String>,
    /// Effective maximum outputs per logical request after bridge fan-out.
    pub max_outputs: u8,
    /// Provider-wide maximum simultaneous app-server image turns.
    pub max_parallel_outputs: u8,
    /// `SQLite` session binding database.
    pub session_database: PathBuf,
    /// Maximum JSONL message bytes.
    pub rpc_max_message_bytes: usize,
    /// Maximum JSONL bytes retained for one app-server notification.
    pub rpc_max_notification_bytes: usize,
    /// Default RPC request timeout in milliseconds.
    pub rpc_timeout_ms: u64,
    /// Notification broadcast capacity.
    pub notification_capacity: usize,
    /// Process shutdown timeout in milliseconds.
    pub shutdown_timeout_ms: u64,
    /// Minimum restart interval in milliseconds.
    pub restart_backoff_ms: u64,
}

impl Default for CodexAppServerSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            executable: PathBuf::from("codex"),
            args: Vec::new(),
            cwd: None,
            codex_model: None,
            max_outputs: 4,
            max_parallel_outputs: 2,
            session_database: PathBuf::from("./data/state.sqlite3"),
            rpc_max_message_bytes: 64 * 1024 * 1024,
            rpc_max_notification_bytes: 48 * 1024 * 1024,
            rpc_timeout_ms: 60_000,
            notification_capacity: 4,
            shutdown_timeout_ms: 5_000,
            restart_backoff_ms: 250,
        }
    }
}

/// Codex OAuth Responses settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CodexResponsesSettings {
    /// Register this provider.
    pub enabled: bool,
    /// Private Responses endpoint.
    pub endpoint: String,
    /// Orchestrating Responses model.
    pub responses_model: String,
    /// Image generation tool model.
    pub image_model: String,
    /// Maximum simultaneous upstream calls within one multi-image request.
    pub max_parallel_outputs: usize,
    /// Maximum attempts for failures that are explicitly safe to retry.
    pub max_transient_attempts: u8,
    /// Base delay between safe transient attempts in milliseconds.
    pub transient_retry_backoff_ms: u64,
}

impl Default for CodexResponsesSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            endpoint: "https://chatgpt.com/backend-api/codex/responses".to_owned(),
            responses_model: "gpt-5.5".to_owned(),
            image_model: "gpt-image-2".to_owned(),
            max_parallel_outputs: 2,
            max_transient_attempts: 2,
            transient_retry_backoff_ms: 750,
        }
    }
}

/// Official `OpenAI` Images settings with indirect credential references.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OpenAiSettings {
    /// Register this provider.
    pub enabled: bool,
    /// Official or compatible API base URL.
    pub base_url: String,
    /// Environment variable containing the API key, never the key itself.
    pub api_key_env: String,
    /// Optional default image model; provider default when absent.
    pub model: Option<String>,
    /// Optional environment variable containing an organization ID.
    pub organization_env: Option<String>,
    /// Optional environment variable containing a project ID.
    pub project_env: Option<String>,
}

impl Default for OpenAiSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: "https://api.openai.com/v1".to_owned(),
            api_key_env: "OPENAI_API_KEY".to_owned(),
            model: None,
            organization_env: Some("OPENAI_ORG_ID".to_owned()),
            project_env: Some("OPENAI_PROJECT_ID".to_owned()),
        }
    }
}

/// HTTP listener, authentication reference, and request bounds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerSettings {
    /// Listener socket address.
    pub bind: String,
    /// Optional environment variable containing bridge bearer auth.
    pub bearer_token_env: Option<String>,
    /// Optional cross-process activation lock acquired before provider bootstrap.
    pub activation_lock: Option<PathBuf>,
    /// Maximum HTTP request body bytes.
    pub max_body_bytes: u64,
    /// Maximum header bytes.
    pub max_header_bytes: usize,
    /// Maximum simultaneous HTTP connections.
    pub max_connections: usize,
    /// Socket read-stall timeout in milliseconds; zero disables it.
    pub read_timeout_ms: u64,
    /// Maximum time a socket write may remain stalled without progress.
    pub write_timeout_ms: u64,
    /// Opt-in low-cardinality Prometheus exposition.
    pub metrics: MetricsSettings,
    /// Structured content-safe tracing for the standalone server.
    pub tracing: TracingSettings,
    /// Durable asynchronous generation job settings.
    pub jobs: JobSettings,
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8787".to_owned(),
            bearer_token_env: None,
            activation_lock: None,
            max_body_bytes: 80 * 1024 * 1024,
            max_header_bytes: 32 * 1024,
            max_connections: 256,
            read_timeout_ms: 0,
            write_timeout_ms: 30_000,
            metrics: MetricsSettings::default(),
            tracing: TracingSettings::default(),
            jobs: JobSettings::default(),
        }
    }
}

/// Durable asynchronous generation job storage and execution bounds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct JobSettings {
    /// Enable durable job and history routes.
    pub enabled: bool,
    /// `SQLite` database for job state and the rebuildable history index.
    pub database: PathBuf,
    /// Maximum queued jobs retained before admission rejects submissions.
    pub max_pending: usize,
    /// Maximum jobs dispatched concurrently by the background worker.
    pub max_running: usize,
    /// Completed job retention in seconds.
    pub retention_secs: u64,
    /// Maximum terminal jobs retained after cleanup.
    pub max_retained: usize,
    /// Maximum logical bytes retained across non-favorite terminal jobs.
    pub max_retained_bytes: u64,
    /// Maximum logical bytes admitted across every durable job row.
    pub max_database_bytes: u64,
}

impl Default for JobSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            database: PathBuf::from("./data/jobs.sqlite3"),
            max_pending: 1_000,
            max_running: 4,
            retention_secs: 7 * 24 * 60 * 60,
            max_retained: 10_000,
            max_retained_bytes: 256 * 1024 * 1024,
            max_database_bytes: 1024 * 1024 * 1024,
        }
    }
}

/// Safe Prometheus endpoint configuration.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsSettings {
    /// Expose authenticated `GET /metrics` on the main listener.
    pub enabled: bool,
}

/// Structured tracing configuration for standalone transports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TracingSettings {
    /// Install a safe INFO-level JSON subscriber in the CLI server process.
    pub enabled: bool,
}

impl Default for TracingSettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}
