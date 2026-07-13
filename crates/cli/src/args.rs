use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use imagegen_bridge::core::{
    AspectRatio, Background, CompatibilityMode, ImageSize, Moderation, MultiImageFailurePolicy,
    NegativePromptMode, OutputFormat, Quality, Resolution, ResponseFormat, RevisedPromptPolicy,
    SessionMode,
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
    /// TOML configuration file. If omitted, ./imagegen-bridge.toml is used when present.
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputMode {
    Human,
    Plain,
    Json,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Generate one or more images.
    Generate(GenerateArgs),
    /// Edit one or more source images.
    Edit(EditArgs),
    /// Run the bounded HTTP API until interrupted.
    Serve(ServeArgs),
    /// Inspect configured providers.
    Providers(ProvidersArgs),
    /// Inspect or delete persistent session bindings.
    Session(SessionArgs),
    /// Validate or inspect effective configuration.
    Config(ConfigArgs),
    /// Manage bridge-owned artifacts.
    Artifacts(ArtifactsArgs),
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
pub(crate) struct GenerateArgs {
    /// Complete native request JSON file, or `-` for stdin.
    #[arg(long, value_name = "FILE")]
    pub request: Option<PathBuf>,

    /// Prompt text, or `-` to read it from stdin.
    #[arg(long, short, required_unless_present = "request", value_name = "TEXT")]
    pub prompt: Option<String>,

    /// Local image used as a visual reference. Repeatable.
    #[arg(long = "reference", value_name = "FILE")]
    pub references: Vec<PathBuf>,

    #[command(flatten)]
    pub image: ImageArgs,

    /// Validate and print the normalized request without starting a provider.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub(crate) struct EditArgs {
    /// Complete native request JSON file, or `-` for stdin.
    #[arg(long, value_name = "FILE")]
    pub request: Option<PathBuf>,

    /// Prompt text, or `-` to read it from stdin.
    #[arg(long, short, required_unless_present = "request", value_name = "TEXT")]
    pub prompt: Option<String>,

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

    /// Validate and print the normalized request without starting a provider.
    #[arg(long)]
    pub dry_run: bool,
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
    /// Moderation behavior.
    #[arg(long, value_parser = parse_moderation)]
    pub moderation: Option<Moderation>,
    /// Number of partial progress images requested.
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=3))]
    pub partial_images: Option<u8>,
    /// Behavior when one output in a multi-image request fails.
    #[arg(long, value_parser = parse_failure_policy)]
    pub failure_policy: Option<MultiImageFailurePolicy>,
    /// Output payload representation.
    #[arg(long, value_parser = parse_response_format)]
    pub response_format: Option<ResponseFormat>,
    /// Safe logical artifact filename prefix.
    #[arg(long)]
    pub filename_prefix: Option<String>,
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
            && self.count.is_none()
            && self.size.is_none()
            && self.aspect_ratio.is_none()
            && self.resolution.is_none()
            && self.quality.is_none()
            && self.format.is_none()
            && self.compression.is_none()
            && self.background.is_none()
            && self.moderation.is_none()
            && self.partial_images.is_none()
            && self.failure_policy.is_none()
            && self.response_format.is_none()
            && self.filename_prefix.is_none()
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
enum_parser!(parse_moderation, Moderation);
enum_parser!(parse_failure_policy, MultiImageFailurePolicy);
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
