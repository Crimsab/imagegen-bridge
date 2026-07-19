use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use imagegen_bridge::core::{
    ArtifactCollisionPolicy, ArtifactMetadataPolicy, AspectRatio, Background, BatchExecution,
    CompatibilityMode, FallbackPolicy, ImageAction, ImageSize, InputFidelity, Moderation,
    MultiImageFailurePolicy, NegativePromptMode, OutputFormat, ProviderRoute, Quality, Resolution,
    ResponseFormat, RevisedPromptPolicy, SessionMode, TransparencyMode,
};

#[derive(Debug, Parser)]
#[command(
    name = "imagegen-bridge",
    version,
    about = "Generate and edit images through Codex OAuth",
    long_about = "A bounded, provider-neutral image generation bridge. Configuration precedence is defaults < file < environment < --set/--unset.",
    disable_help_subcommand = true,
    propagate_version = true
)]
pub(crate) struct Cli {
    /// TOML file; otherwise use ./imagegen-bridge.toml, then the XDG user config.
    #[arg(
        long,
        global = true,
        value_name = "FILE",
        env = "IMAGEGEN_BRIDGE_CONFIG"
    )]
    pub config: Option<PathBuf>,

    /// Highest-precedence dotted configuration override (`KEY=TOML_VALUE`).
    #[arg(long = "set", global = true, value_name = "KEY=VALUE")]
    pub set: Vec<String>,

    /// Clear an optional configuration field at highest precedence.
    #[arg(long = "unset", global = true, value_name = "KEY")]
    pub unset: Vec<String>,

    /// Emit stable JSON on stdout and JSON errors on stderr.
    #[arg(long, global = true, conflicts_with = "plain")]
    pub json: bool,

    /// Emit compact line-oriented text.
    #[arg(long, global = true, conflicts_with = "json")]
    pub plain: bool,

    /// Suppress non-essential human output.
    #[arg(long, short, global = true)]
    pub quiet: bool,

    /// Permit inline base64 image bodies on an interactive terminal.
    #[arg(long, global = true)]
    pub allow_inline: bool,

    /// Wrap JSON image results with verified absolute local artifact paths.
    #[arg(long, global = true, requires = "json")]
    pub local_artifact_paths: bool,

    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    pub(crate) const fn output_mode(&self) -> OutputMode {
        if self.json {
            OutputMode::Json
        } else if self.plain {
            OutputMode::Plain
        } else {
            OutputMode::Human
        }
    }

    pub(crate) fn allows_passive_update_check(&self) -> bool {
        matches!(self.output_mode(), OutputMode::Human)
            && !self.quiet
            && !matches!(
                self.command,
                Command::Serve(_)
                    | Command::Gateway(_)
                    | Command::Dashboard(_)
                    | Command::Update(_)
                    | Command::Completions(_)
                    | Command::Man(_)
                    | Command::Schema(_)
            )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputMode {
    Human,
    Plain,
    Json,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Configure a local Codex OAuth installation safely and idempotently.
    Setup(SetupArgs),
    /// Run complete installation, storage, provider, and optional live checks.
    Doctor(DoctorArgs),
    /// Generate one or more images.
    Generate(GenerateArgs),
    /// Edit one or more source images.
    Edit(EditArgs),
    /// Detect and remove a flat image background without calling a provider.
    Background(BackgroundArgs),
    /// Run the bounded HTTP API until interrupted.
    Serve(ServeArgs),
    /// Run the stable active/passive deployment gateway without OAuth access.
    Gateway(GatewayArgs),
    /// Open, attach to, or start the local embedded dashboard.
    Dashboard(DashboardArgs),
    /// Inspect configured providers.
    Providers(ProvidersArgs),
    /// Inspect or delete persistent session bindings.
    Session(SessionArgs),
    /// Create, inspect, update, or delete reusable generation presets.
    Preset(PresetArgs),
    /// Validate or inspect effective configuration.
    Config(ConfigArgs),
    /// Manage bridge-owned artifacts.
    Artifacts(ArtifactsArgs),
    /// Check for, install, or roll back Imagegen Bridge releases.
    Update(UpdateArgs),
    /// Verify provider authentication without generating an image.
    AuthDoctor(AuthDoctorArgs),
    /// Print or verify generated wire schemas.
    Schema(SchemaArgs),
    /// Generate shell completion definitions.
    Completions(CompletionsArgs),
    /// Generate a manual page.
    Man(ManArgs),
}

#[derive(Debug, Args)]
pub(crate) struct GatewayArgs {
    /// Gateway listener exposed to clients.
    #[arg(long, default_value = "127.0.0.1:8787")]
    pub bind: String,
    /// Internal blue backend base URL.
    #[arg(long, default_value = "http://imagegen-bridge-blue:8787")]
    pub blue: String,
    /// Internal green backend base URL.
    #[arg(long, default_value = "http://imagegen-bridge-green:8787")]
    pub green: String,
    /// Coordination file containing `blue`, `green`, or `hold`.
    #[arg(long, default_value = "/coord/active-slot")]
    pub state_file: PathBuf,
    /// Maximum time a request may wait during a planned handoff.
    #[arg(long, default_value_t = 2_100_000)]
    pub hold_timeout_ms: u64,
    /// Backend readiness polling interval while held.
    #[arg(long, default_value_t = 250)]
    pub probe_interval_ms: u64,
    /// Maximum simultaneous gateway requests, including held requests.
    #[arg(long, default_value_t = 256)]
    pub max_connections: usize,
    /// Maximum streamed request body bytes.
    #[arg(long, default_value_t = 80 * 1024 * 1024)]
    pub max_body_bytes: usize,
    /// Maximum time from backend dispatch through the complete response stream.
    #[arg(long, default_value_t = 1_860_000)]
    pub forward_timeout_ms: u64,
    /// Maximum idle time between response body chunks.
    #[arg(long, default_value_t = 60_000)]
    pub response_idle_timeout_ms: u64,
}

#[derive(Debug, Args)]
pub(crate) struct UpdateArgs {
    #[command(subcommand)]
    pub command: UpdateCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum UpdateCommand {
    /// Check GitHub Releases without changing the installation.
    Check,
    /// Replace this standalone binary with the latest verified release.
    Install {
        /// Show the update target and backup plan without downloading or replacing the binary.
        #[arg(long, short = 'n')]
        dry_run: bool,
        /// Confirm replacement without prompting.
        #[arg(long)]
        yes: bool,
    },
    /// Update a Compose deployment and its pinned `IMAGEGEN_BRIDGE_IMAGE` value.
    Docker {
        /// Compose file containing the imagegen-bridge service.
        #[arg(long, default_value = "compose.package.yaml", value_name = "FILE")]
        compose_file: PathBuf,
        /// Environment file whose `IMAGEGEN_BRIDGE_IMAGE` pin will be updated atomically.
        #[arg(long, default_value = ".env", value_name = "FILE")]
        env_file: PathBuf,
        /// Show the pull/recreate plan without modifying files or containers.
        #[arg(long, short = 'n')]
        dry_run: bool,
        /// Confirm the image pin change and Compose recreation.
        #[arg(long)]
        yes: bool,
        /// Use the bundled gateway and mutually exclusive blue/green slots.
        #[arg(long)]
        active_passive: bool,
        /// Host path mounted into the gateway as `/coord/active-slot`.
        #[arg(long, default_value = "deploy/coord/active-slot", value_name = "FILE")]
        coordination_file: PathBuf,
        /// Persistent host file recording the last verified active slot.
        #[arg(
            long,
            default_value = ".imagegen-bridge-active-slot",
            value_name = "FILE"
        )]
        slot_file: PathBuf,
        /// Maximum seconds to wait for the new slot readiness gate.
        #[arg(long, default_value_t = 180)]
        readiness_timeout_secs: u64,
    },
    /// Restore the previous standalone binary retained by the last update.
    Rollback {
        /// Show the rollback plan without replacing the binary.
        #[arg(long, short = 'n')]
        dry_run: bool,
        /// Confirm rollback without prompting.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Args)]
pub(crate) struct BackgroundArgs {
    #[command(subcommand)]
    pub command: BackgroundCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum BackgroundCommand {
    /// Convert a chroma-key background to validated PNG/WebP alpha.
    Remove {
        /// Source PNG, JPEG, or WebP image.
        input: PathBuf,
        /// Destination `.png` or `.webp` file.
        #[arg(long, short = 'o')]
        output: PathBuf,
        /// `auto` samples the border; otherwise use #RRGGBB.
        #[arg(long, default_value = "auto")]
        key: String,
        /// Chroma distance at or below which pixels become transparent.
        #[arg(long, default_value_t = 12, value_parser = clap::value_parser!(u8).range(0..=254))]
        transparent_threshold: u8,
        /// Chroma distance at or above which pixels become opaque.
        #[arg(long, default_value_t = 96, value_parser = clap::value_parser!(u8).range(1..=255))]
        opaque_threshold: u8,
        /// Preserve key-color spill on antialiased edges.
        #[arg(long)]
        no_despill: bool,
        /// Atomically replace an existing destination.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Args)]
pub(crate) struct SetupArgs {
    /// Directory containing bridge runtime state and the session database.
    #[arg(long, value_name = "DIR")]
    pub state_root: Option<PathBuf>,
    /// Directory used for generated bridge-owned artifacts.
    #[arg(long, value_name = "DIR")]
    pub output_root: Option<PathBuf>,
    /// Apply the displayed plan without interactive confirmation.
    #[arg(long)]
    pub yes: bool,
    /// Never prompt; changes require --yes and missing choices fail safely.
    #[arg(long, visible_alias = "no-input")]
    pub non_interactive: bool,
    /// Print the complete plan without writing files or starting providers.
    #[arg(long, short = 'n')]
    pub dry_run: bool,
    /// After setup, perform one explicitly confirmed paid OAuth generation.
    #[arg(long)]
    pub live_probe: bool,
}

#[derive(Debug, Args)]
pub(crate) struct DoctorArgs {
    /// Limit provider readiness and capability checks to one provider.
    #[arg(long)]
    pub provider: Option<String>,
    /// Perform one explicitly confirmed paid OAuth generation.
    #[arg(long)]
    pub live_probe: bool,
    /// Confirm the explicitly requested live probe without prompting.
    #[arg(long)]
    pub yes: bool,
    /// Never prompt; a requested live probe then requires --yes.
    #[arg(long, visible_alias = "no-input")]
    pub non_interactive: bool,
}

#[derive(Debug, Args)]
#[group(id = "generate_prompt", required = false, multiple = false)]
pub(crate) struct GenerateArgs {
    /// Complete native request JSON file, or `-` for stdin.
    #[arg(long, value_name = "FILE", group = "generate_prompt")]
    pub request: Option<PathBuf>,

    /// Prompt text. This positional form is the preferred interactive syntax.
    #[arg(value_name = "PROMPT", group = "generate_prompt")]
    pub prompt_text: Option<String>,

    /// Prompt text, or `-` to read it from stdin; retained for scripts.
    #[arg(long, short, value_name = "TEXT", group = "generate_prompt")]
    pub prompt: Option<String>,

    /// Apply reusable configuration before explicit image flags.
    #[arg(long, value_name = "NAME", conflicts_with = "request")]
    pub preset: Option<String>,

    /// Local image used as a visual reference. Repeatable.
    #[arg(long = "reference", value_name = "FILE")]
    pub references: Vec<PathBuf>,

    #[command(flatten)]
    pub image: ImageArgs,

    #[command(flatten)]
    pub presentation: PresentationArgs,

    /// Validate and print the normalized request without starting a provider.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
#[group(id = "edit_prompt", required = false, multiple = false)]
pub(crate) struct EditArgs {
    /// Complete native request JSON file, or `-` for stdin.
    #[arg(long, value_name = "FILE", group = "edit_prompt")]
    pub request: Option<PathBuf>,

    /// Prompt text. This positional form is the preferred interactive syntax.
    #[arg(value_name = "PROMPT", group = "edit_prompt")]
    pub prompt_text: Option<String>,

    /// Prompt text, or `-` to read it from stdin; retained for scripts.
    #[arg(long, short, value_name = "TEXT", group = "edit_prompt")]
    pub prompt: Option<String>,

    /// Apply reusable edit configuration before explicit image flags.
    #[arg(long, value_name = "NAME", conflicts_with = "request")]
    pub preset: Option<String>,

    /// Source image to edit. Repeatable.
    #[arg(
        long,
        short = 'i',
        required_unless_present = "request",
        value_name = "FILE"
    )]
    pub images: Vec<PathBuf>,

    /// Optional edit mask.
    #[arg(long, value_name = "FILE")]
    pub mask: Option<PathBuf>,

    /// Additional visual reference. Repeatable.
    #[arg(long = "reference", value_name = "FILE")]
    pub references: Vec<PathBuf>,

    #[command(flatten)]
    pub image: ImageArgs,

    #[command(flatten)]
    pub presentation: PresentationArgs,

    /// Validate and print the normalized request without starting a provider.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args, Default, Clone, Copy)]
pub(crate) struct PresentationArgs {
    /// Open every generated artifact or URL with the system viewer.
    #[arg(long)]
    pub open: bool,
    /// Render artifacts in supported Kitty/iTerm2-compatible terminals.
    #[arg(long)]
    pub preview: bool,
}

impl PresentationArgs {
    pub(crate) const fn requested(self) -> bool {
        self.open || self.preview
    }
}

#[derive(Debug, Args, Default)]
pub(crate) struct ImageArgs {
    /// Negative prompt text.
    #[arg(long)]
    pub negative_prompt: Option<String>,
    /// Provider name; defaults to configuration.
    #[arg(long)]
    pub provider: Option<String>,
    /// Provider-specific model selection.
    #[arg(long)]
    pub model: Option<String>,
    /// Ordered fallback route as PROVIDER or PROVIDER:MODEL. Repeatable.
    #[arg(long = "fallback", value_name = "PROVIDER[:MODEL]", value_parser = parse_provider_route)]
    pub fallbacks: Vec<ProviderRoute>,
    /// Conditions under which the next fallback route may run.
    #[arg(long, value_parser = parse_fallback_policy)]
    pub fallback_policy: Option<FallbackPolicy>,
    /// Number of images.
    #[arg(long = "count", short = 'n', value_parser = clap::value_parser!(u8).range(1..))]
    pub count: Option<u8>,
    /// `auto` or `WIDTHxHEIGHT`.
    #[arg(long)]
    pub size: Option<ImageSize>,
    /// Ratio such as `1:1`, `3:2`, or `16:9`.
    #[arg(long)]
    pub aspect_ratio: Option<AspectRatio>,
    /// Coarse output resolution.
    #[arg(long, value_parser = parse_resolution)]
    pub resolution: Option<Resolution>,
    /// Generation quality.
    #[arg(long, value_parser = parse_quality)]
    pub quality: Option<Quality>,
    /// Encoded output format.
    #[arg(long = "format", value_parser = parse_format)]
    pub format: Option<OutputFormat>,
    /// JPEG/WebP compression from 0 through 100.
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=100))]
    pub compression: Option<u8>,
    /// Background behavior.
    #[arg(long, value_parser = parse_background)]
    pub background: Option<Background>,
    /// Transparent result strategy: auto, native, or `chroma_key`.
    #[arg(long, value_parser = parse_transparency_mode)]
    pub transparency: Option<TransparencyMode>,
    /// Explicit chroma key as #RRGGBB; otherwise selected from the prompt.
    #[arg(long, value_name = "#RRGGBB")]
    pub chroma_key: Option<String>,
    /// Chroma distance at or below which pixels become transparent.
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=254))]
    pub chroma_transparent_threshold: Option<u8>,
    /// Chroma distance at or above which pixels become opaque.
    #[arg(long, value_parser = clap::value_parser!(u8).range(1..=255))]
    pub chroma_opaque_threshold: Option<u8>,
    /// Preserve key-color spill instead of cleaning antialiased edges.
    #[arg(long)]
    pub no_despill: bool,
    /// Moderation behavior.
    #[arg(long, value_parser = parse_moderation)]
    pub moderation: Option<Moderation>,
    /// Number of partial progress images requested.
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=3))]
    pub partial_images: Option<u8>,
    /// Behavior when one output in a multi-image request fails.
    #[arg(long, value_parser = parse_failure_policy)]
    pub failure_policy: Option<MultiImageFailurePolicy>,
    /// Automatic, sequential, or bounded-parallel fan-out execution.
    #[arg(long, value_parser = parse_batch_execution)]
    pub batch_execution: Option<BatchExecution>,
    /// Input-image fidelity for edit/reference operations.
    #[arg(long, value_parser = parse_input_fidelity)]
    pub input_fidelity: Option<InputFidelity>,
    /// Image tool action for conversational transports.
    #[arg(long, value_parser = parse_image_action)]
    pub action: Option<ImageAction>,
    /// Output payload representation.
    #[arg(long, value_parser = parse_response_format)]
    pub response_format: Option<ResponseFormat>,
    /// Safe logical artifact filename prefix.
    #[arg(long)]
    pub filename_prefix: Option<String>,
    /// Exact single-image path below the configured artifact root.
    #[arg(
        long = "output",
        short = 'o',
        value_name = "FILE",
        conflicts_with = "output_dir"
    )]
    pub output_path: Option<PathBuf>,
    /// Per-call directory below the configured artifact root.
    #[arg(long, value_name = "DIR", conflicts_with = "output_path")]
    pub output_dir: Option<PathBuf>,
    /// Atomic behavior when an explicit output filename already exists.
    #[arg(long, value_parser = parse_collision)]
    pub collision: Option<ArtifactCollisionPolicy>,
    /// Persist portable generation metadata beside or inside each image.
    #[arg(long, value_parser = parse_metadata)]
    pub metadata: Option<ArtifactMetadataPolicy>,
    /// Provider compatibility behavior.
    #[arg(long, value_parser = parse_compatibility)]
    pub compatibility: Option<CompatibilityMode>,
    /// Negative-prompt handling behavior.
    #[arg(long, value_parser = parse_negative_prompt_mode)]
    pub negative_prompt_mode: Option<NegativePromptMode>,
    /// Revised-prompt visibility/requirement policy.
    #[arg(long, value_parser = parse_revised_prompt)]
    pub revised_prompt: Option<RevisedPromptPolicy>,
    /// Conversation behavior.
    #[arg(long, value_parser = parse_session_mode)]
    pub session: Option<SessionMode>,
    /// Durable caller-selected session binding key.
    #[arg(long)]
    pub session_key: Option<String>,
    /// Existing explicit Codex thread ID.
    #[arg(long)]
    pub thread_id: Option<String>,
    /// Caller-selected idempotency key.
    #[arg(long)]
    pub idempotency_key: Option<String>,
    /// Per-request deadline in milliseconds.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    pub timeout_ms: Option<u64>,
    /// Opaque end-user identifier, forwarded only when supported.
    #[arg(long)]
    pub user: Option<String>,
}

impl ImageArgs {
    pub(crate) fn is_empty(&self) -> bool {
        self.negative_prompt.is_none()
            && self.provider.is_none()
            && self.model.is_none()
            && self.fallbacks.is_empty()
            && self.fallback_policy.is_none()
            && self.count.is_none()
            && self.size.is_none()
            && self.aspect_ratio.is_none()
            && self.resolution.is_none()
            && self.quality.is_none()
            && self.format.is_none()
            && self.compression.is_none()
            && self.background.is_none()
            && self.transparency.is_none()
            && self.chroma_key.is_none()
            && self.chroma_transparent_threshold.is_none()
            && self.chroma_opaque_threshold.is_none()
            && !self.no_despill
            && self.moderation.is_none()
            && self.partial_images.is_none()
            && self.failure_policy.is_none()
            && self.batch_execution.is_none()
            && self.input_fidelity.is_none()
            && self.action.is_none()
            && self.response_format.is_none()
            && self.filename_prefix.is_none()
            && self.output_path.is_none()
            && self.output_dir.is_none()
            && self.collision.is_none()
            && self.metadata.is_none()
            && self.compatibility.is_none()
            && self.negative_prompt_mode.is_none()
            && self.revised_prompt.is_none()
            && self.session.is_none()
            && self.session_key.is_none()
            && self.thread_id.is_none()
            && self.idempotency_key.is_none()
            && self.timeout_ms.is_none()
            && self.user.is_none()
    }
}

#[derive(Debug, Args)]
pub(crate) struct ServeArgs {
    /// Override the configured numeric bind address.
    #[arg(long, value_name = "IP:PORT")]
    pub bind: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct DashboardArgs {
    /// Override the local listener; only loopback IP addresses are accepted.
    #[arg(long, value_name = "IP:PORT")]
    pub bind: Option<String>,
    /// Open the dashboard with the system browser even when non-interactive.
    #[arg(long, conflicts_with = "no_open")]
    pub open: bool,
    /// Never open a browser; print connection details only.
    #[arg(long)]
    pub no_open: bool,
    /// Attach to an existing bridge or fail without starting one.
    #[arg(long)]
    pub attach_only: bool,
}

#[derive(Debug, Args)]
pub(crate) struct ProvidersArgs {
    #[command(subcommand)]
    pub command: ProviderCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ProviderCommand {
    /// List configured provider descriptors.
    List,
    /// Print dynamic capabilities for a provider/model.
    Capabilities {
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        model: Option<String>,
    },
    /// Run non-generating provider readiness checks.
    Readiness,
}

#[derive(Debug, Args)]
pub(crate) struct SessionArgs {
    #[command(subcommand)]
    pub command: SessionCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum SessionCommand {
    /// Look up one persistent binding.
    Get {
        key: String,
        #[arg(long)]
        provider: Option<String>,
    },
    /// Delete one persistent binding.
    Delete {
        key: String,
        #[arg(long)]
        provider: Option<String>,
        /// Report the operation without deleting anything.
        #[arg(long)]
        dry_run: bool,
        /// Confirm this destructive non-interactive operation.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Args)]
pub(crate) struct PresetArgs {
    #[command(subcommand)]
    pub command: PresetCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum PresetCommand {
    /// List stored presets in stable name order.
    List {
        /// Maximum presets to return.
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u8).range(1..=100))]
        limit: u8,
    },
    /// Print one complete preset.
    Get {
        /// Stable preset name.
        name: String,
    },
    /// Create a preset from an `ImagePresetTemplate` or native `ImageRequest` JSON file.
    Create {
        /// Stable preset name.
        name: String,
        /// JSON file, or `-` for stdin.
        #[arg(long = "from", value_name = "FILE")]
        source: PathBuf,
        /// Optional human explanation.
        #[arg(long)]
        description: Option<String>,
    },
    /// Fully replace an existing preset from JSON.
    Update {
        /// Stable preset name.
        name: String,
        /// JSON file, or `-` for stdin.
        #[arg(long = "from", value_name = "FILE")]
        source: PathBuf,
        /// Optional replacement explanation; omit to clear it.
        #[arg(long)]
        description: Option<String>,
    },
    /// Delete a stored preset.
    Delete {
        /// Stable preset name.
        name: String,
        /// Report the operation without deleting anything.
        #[arg(long)]
        dry_run: bool,
        /// Confirm this destructive non-interactive operation.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Args)]
pub(crate) struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommand,
}

#[derive(Debug, Clone, Copy, Subcommand)]
pub(crate) enum ConfigCommand {
    /// Perform complete non-mutating validation.
    Check,
    /// Print the effective redaction-safe configuration.
    Show,
    /// Print per-field configuration provenance without values.
    Origins,
}

#[derive(Debug, Args)]
pub(crate) struct ArtifactsArgs {
    #[command(subcommand)]
    pub command: ArtifactCommand,
}

#[derive(Debug, Clone, Copy, Subcommand)]
pub(crate) enum ArtifactCommand {
    /// Delete only verified bridge-owned artifacts under configured retention policy.
    Cleanup {
        /// Describe policy without opening or mutating the artifact store.
        #[arg(long)]
        dry_run: bool,
        /// Confirm this destructive non-interactive operation.
        #[arg(long)]
        force: bool,
    },
    /// Audit or repair missing artifact/sidecar ownership records.
    Repair {
        /// Inspect and report repairable records without mutating storage.
        #[arg(long, conflicts_with = "force")]
        dry_run: bool,
        /// Apply only conservative ownership repairs.
        #[arg(long, conflicts_with = "dry_run")]
        force: bool,
    },
}

#[derive(Debug, Args)]
pub(crate) struct CompletionsArgs {
    /// Target shell.
    #[arg(value_enum)]
    pub shell: Shell,
}

#[derive(Debug, Args)]
pub(crate) struct AuthDoctorArgs {
    /// Limit the check to one provider.
    #[arg(long)]
    pub provider: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct SchemaArgs {
    /// Schema document to generate.
    #[arg(long, value_enum, default_value = "json-schema")]
    pub kind: SchemaKind,
    /// Output file, or `-` for stdout.
    #[arg(long, short, default_value = "-", conflicts_with = "check")]
    pub output: PathBuf,
    /// Compare generated output with a bounded regular file.
    #[arg(long, value_name = "FILE")]
    pub check: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum SchemaKind {
    JsonSchema,
    Openapi,
}

#[derive(Debug, Args)]
pub(crate) struct ManArgs {
    /// Output file, or `-` for stdout.
    #[arg(long, short, default_value = "-")]
    pub output: PathBuf,
}

macro_rules! enum_parser {
    ($function:ident, $type:ty) => {
        fn $function(value: &str) -> Result<$type, String> {
            let encoded = serde_json::to_string(value).map_err(|error| error.to_string())?;
            serde_json::from_str(&encoded).map_err(|_| format!("unsupported value `{value}`"))
        }
    };
}

enum_parser!(parse_resolution, Resolution);
enum_parser!(parse_quality, Quality);
enum_parser!(parse_format, OutputFormat);
enum_parser!(parse_background, Background);
enum_parser!(parse_transparency_mode, TransparencyMode);
enum_parser!(parse_fallback_policy, FallbackPolicy);
enum_parser!(parse_batch_execution, BatchExecution);

fn parse_provider_route(value: &str) -> Result<ProviderRoute, String> {
    let (provider, model) = value
        .split_once(':')
        .map_or((value, None), |(provider, model)| (provider, Some(model)));
    if provider.is_empty() || model.is_some_and(str::is_empty) {
        return Err("fallback must use PROVIDER or PROVIDER:MODEL syntax".to_owned());
    }
    Ok(ProviderRoute {
        provider: provider.to_owned(),
        model: model.map(str::to_owned),
    })
}
enum_parser!(parse_moderation, Moderation);
enum_parser!(parse_failure_policy, MultiImageFailurePolicy);
enum_parser!(parse_input_fidelity, InputFidelity);
enum_parser!(parse_image_action, ImageAction);
enum_parser!(parse_collision, ArtifactCollisionPolicy);
enum_parser!(parse_metadata, ArtifactMetadataPolicy);
enum_parser!(parse_response_format, ResponseFormat);
enum_parser!(parse_compatibility, CompatibilityMode);
enum_parser!(parse_negative_prompt_mode, NegativePromptMode);
enum_parser!(parse_revised_prompt, RevisedPromptPolicy);
enum_parser!(parse_session_mode, SessionMode);

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use clap::CommandFactory as _;

    use super::*;

    #[test]
    fn clap_contract_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_full_generation_surface() {
        let cli = Cli::try_parse_from([
            "imagegen-bridge",
            "generate",
            "--prompt",
            "hello",
            "--size",
            "1024x1024",
            "--aspect-ratio",
            "16:9",
            "--resolution",
            "2k",
            "--quality",
            "high",
            "--format",
            "webp",
            "--compression",
            "80",
            "--background",
            "transparent",
            "--moderation",
            "low",
            "--count",
            "3",
            "--failure-policy",
            "best_effort",
            "--action",
            "generate",
            "--response-format",
            "artifact",
            "--session",
            "persistent",
            "--session-key",
            "cli-test",
            "--dry-run",
            "--json",
        ])
        .expect("CLI should parse");
        assert!(matches!(cli.command, Command::Generate(_)));
        assert_eq!(cli.output_mode(), OutputMode::Json);
    }
}
