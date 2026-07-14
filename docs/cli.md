# Command-line interface

The `imagegen-bridge` binary uses the same normalized contract, layered
configuration, provider registry, runtime limits, and artifact verification as
the Rust facade and HTTP server. It never requires an `OPENAI_API_KEY` for the
Codex providers; they inherit the existing Codex OAuth login.

Build the binary from a local checkout with:

```sh
cargo build --release -p imagegen-bridge-cli
./target/release/imagegen-bridge setup
./target/release/imagegen-bridge doctor
```

## Guided setup and diagnostics

`setup` is idempotent and safe to rerun. It detects the Codex executable and
ChatGPT OAuth login, chooses XDG-compatible config/state/data locations,
previews all changes, writes configuration atomically, protects the state
directory, and initializes the session database. It does not copy OAuth tokens.

```sh
imagegen-bridge setup --dry-run
imagegen-bridge setup
imagegen-bridge setup --yes --non-interactive --json
imagegen-bridge setup \
  --config ./bridge.toml \
  --state-root ./data/state \
  --output-root ./data/artifacts \
  --yes --non-interactive
```

Interactive mutations require confirmation. Machine output modes never prompt;
use `--yes` to apply their displayed plan. A missing or incomplete installation
can be repaired by rerunning the same command. No paid request occurs unless
`--live-probe` is present, and that probe requires a separate confirmation (or
an explicit `--yes`).

`doctor` checks the bridge version, config file and schema, Codex version,
protected OAuth file, storage permissions, SQLite migration version, configured
port, provider readiness, and capability discovery:

```sh
imagegen-bridge doctor
imagegen-bridge doctor --provider codex-responses --json
imagegen-bridge doctor --live-probe
imagegen-bridge doctor --live-probe --yes --non-interactive --json
```

The normal doctor path is non-generating. The live probe produces exactly one
image, verifies it through the ordinary runtime, returns dimensions/format and
elapsed time, and does not retain image bytes because it requests metadata
output.

## Generation and editing

Every normalized request field is available either through a complete native
JSON request or ergonomic flags:

```sh
imagegen-bridge generate \
  "A red origami fox on warm gray" \
  --quality auto \
  --size auto \
  --output illustrations/fox.png \
  --collision suffix

imagegen-bridge edit \
  "Change the jacket to blue" \
  --image ./portrait.png \
  --reference ./palette.png \
  --response-format artifact

imagegen-bridge generate --request request.json --json
imagegen-bridge generate --request - --json < request.json
imagegen-bridge generate --prompt - --dry-run --json < prompt.txt
imagegen-bridge generate "a red-haired woman" --dry-run --json

imagegen-bridge generate \
  "Four distinct editorial variations of a red-haired woman" \
  --count 4 \
  --failure-policy best_effort \
  --output-dir batches/red-haired-woman
```

`--request` is lossless and exclusive: it cannot be mixed with prompt, input,
or parameter flags. `--dry-run` validates and prints the normalized request
without opening artifact/session storage or starting Codex.

`--count N` requests multiple outputs. A provider may return them natively or
the bridge may fan out into bounded upstream calls; inspect `providers
capabilities --json` and its `batching` field for the exact behavior. Isolated
Codex app-server batches can run concurrently. `--session-key` and
`--thread-id` batches are intentionally sequential so turns on one conversation
remain ordered.
Use `--batch-execution sequential` for one upstream fan-out call at a time or
`--batch-execution parallel` to require configured bounded concurrency.
`auto` is the default and becomes sequential for conversational sessions.

`-o, --output FILE` selects an exact filename and is valid only when `n=1`.
`--output-dir DIR` retains generated UUID filenames inside a per-call directory.
Relative paths are interpreted below `artifacts.root`; an absolute CLI path is
accepted only when it resolves lexically below that root. Directories and
filenames use a portable ASCII contract and cannot contain hidden components,
backslashes, traversal, or an extension that disagrees with `--format`.
Publication never overwrites: `--collision error` is the default, while
`--collision suffix` atomically selects `name-2.png`, `name-3.png`, and so on.
Supplying either path option changes the default `b64_json` response to
`artifact`; an explicitly incompatible response mode fails validation.

`--metadata sidecar` selects artifact delivery when no response format was
explicitly chosen and opts into a portable `metadata-<artifact-id>.json` file in
the same directory as each image. It contains the original/effective prompt,
negative prompt, operation summary without input paths or bytes,
requested/effective parameters, normalization records, revised prompt,
provider/model, ordered fallback attempts, usage, session, timings, warnings,
and independently verified image properties. The response exposes its relative
`metadata_name`; cleanup
verifies and removes the owned image and sidecar together. Sidecars are off by
default because prompt and session content may be sensitive.

`--metadata embedded` writes a bounded XMP record directly into PNG, JPEG, or
WebP bytes without re-encoding pixels. It can remain in the default `b64_json`
response or accompany artifact/URL delivery; metadata-only output is rejected.
The final response checksum covers the image with XMP included.
`--metadata sidecar_and_embedded` writes both forms and implicitly selects
artifact delivery. Embedded records include prompt and session content and are
therefore just as privacy-sensitive as sidecars. Combined prompt text above 12
KiB is rejected before provider work. Oversized response-only fields are named
in `omitted_fields` rather than silently truncated. The Rust artifact library
exposes `extract_embedded_metadata` with explicit image limits for verified,
bounded extraction.

`--preview` selects artifact output when no response format was explicitly
chosen and renders PNG through Kitty graphics or supported image formats through
the iTerm2 inline protocol (also used by WezTerm). Unsupported formats,
terminals, and redirected stdout receive a clear fallback message without
turning a successful generation into a failure.
Preview is rejected with `--json`/machine output so control sequences can never
corrupt the wire stream. `--open` launches each artifact or bridge URL with the
platform viewer (`xdg-open`, `open`, or Windows `start`). Viewer launch failures
are reported as command failures; artifact paths are canonicalized and checked
against the configured root first.

Advanced flags include `--negative-prompt`, `--negative-prompt-mode`,
`--revised-prompt`, `--aspect-ratio`, `--resolution`, `--compression`,
`--background`, `--transparency`, `--chroma-key`, both chroma thresholds,
`--no-despill`, `--fallback`, `--fallback-policy`, `--batch-execution`,
`--moderation`, `--partial-images`, `--failure-policy`,
`--input-fidelity`, `--action`, `--compatibility`, `--output`, `--output-dir`,
`--collision`, `--metadata`, `--open`, `--preview`,
`--session`, `--session-key`, `--thread-id`, `--idempotency-key`, and
`--timeout-ms`. Availability is provider/model-specific; inspect it before
requesting strict parameters:

```sh
imagegen-bridge providers list --json
imagegen-bridge providers capabilities --json
imagegen-bridge providers readiness --json
```

Image-generation parameters covered by provider capabilities fail before
provider work when strict and unsupported. `--compatibility normalize` or
`best_effort` permits only the explicit fallbacks reported in
`normalizations`. The native contract also accepts an opaque `--user` field.
Both current Codex transports reject it before provider work because neither
upstream path proves that it consumes this attribution; the field is never
silently discarded, including in `best_effort` mode.

`--failure-policy fail_fast` is the default for multiple outputs.
`--failure-policy best_effort` keeps successful outputs and reports failed
indices in `failures`; this is separate from provider compatibility
`--compatibility best_effort`.

`--background transparent --transparency auto` prefers native alpha and
otherwise uses the bridge's local chroma-key processor. The processor chooses
a key color from the prompt, asks the model for a uniform keyed background,
samples the actual output border, creates a soft alpha matte, despills edges,
and rejects a missing/pathological matte. `native` requires provider support;
`chroma_key` forces local processing. PNG and WebP preserve alpha. The offline
equivalent is `background remove INPUT --output OUTPUT --key auto`.

`--fallback PROVIDER[:MODEL]` is repeatable and ordered. The default
`on_unavailable` policy reroutes only unavailable/unsupported conditions.
`on_error` also permits errors with a known upstream outcome. Safety refusals,
permission errors, cancellation, session errors, and unknown outcomes never
reroute. Fallback requires an isolated session and successful responses expose
the full route trace in `attempts`.

`--input-fidelity low|high` requires at least one source/reference image.
`gpt-image-2` accepts only `high` because its inputs are always processed at
high fidelity. `--action edit` likewise requires image context, while
`--action generate` cannot be combined with the native edit operation. Masks
are rejected during capability negotiation on both current Codex transports.

## Sessions and server

`--session-key NAME` implies persistent mode and reuses the same app-server
thread on later calls. `--thread-id ID` implies explicit-thread mode.

```sh
imagegen-bridge session get gallery --json
imagegen-bridge session delete gallery --dry-run --json
imagegen-bridge session delete gallery --force --json
imagegen-bridge serve
imagegen-bridge serve --bind 127.0.0.1:9000
imagegen-bridge dashboard
imagegen-bridge dashboard --no-open
imagegen-bridge dashboard --attach-only --json
```

Session deletion and artifact cleanup require `--force`; their `--dry-run`
forms are non-mutating. `serve` reports its listener on stderr and stops
gracefully on Ctrl-C/SIGINT. With durable jobs enabled it also reports the
embedded dashboard URL, normally `http://127.0.0.1:8787/dashboard`.

Artifact ownership repair is also explicit and bounded. Stop the bridge before
applying it, audit first, then confirm the same conservative pass:

```sh
imagegen-bridge artifacts repair --dry-run --json
imagegen-bridge artifacts repair --force --json
```

Repair removes a valid ownership record only when its artifact is absent. It
may also remove an unchanged owned sidecar attached to that missing artifact,
or clear a sidecar reference when the artifact is valid but the sidecar is
absent. Changed artifacts, changed sidecars, invalid markers, symlinks, and
unowned files are reported as skipped and never modified.

`dashboard` is the local UI launcher. It first probes the configured loopback
address and, when that endpoint is an Imagegen Bridge dashboard, prints its
connection details and exits. Otherwise it starts the same API and embedded UI
in the foreground, choosing an available loopback port if the configured port
is occupied by an unrelated process. An explicit `--bind IP:PORT` never falls
back and accepts loopback IPs only. `--attach-only` prohibits startup;
`--no-open` prohibits browser launch; `--open` requests it even without an
interactive terminal. Automatic browser opening occurs only for human output
on an interactive terminal.

JSON connection output has stable `mode`, `url`, `api_base_url`, `bind`,
`authentication`, and `opened` fields. A launcher-owned `pid` is present only
when the command started the server. Attached authentication is reported as
`unknown` because the public HTML shell intentionally cannot disclose server
policy. Plain mode emits one `key=value` per line and never includes secrets.

## Configuration, diagnostics, and schemas

Configuration precedence is:

```text
defaults < --config TOML < IMAGEGEN_BRIDGE__* environment < --set/--unset
```

When `--config` is omitted, the CLI checks `./imagegen-bridge.toml` first and
then the XDG user configuration (`$XDG_CONFIG_HOME/imagegen-bridge/config.toml`,
falling back to `~/.config/imagegen-bridge/config.toml`).

```sh
imagegen-bridge config check --json
imagegen-bridge config show --json
imagegen-bridge config origins --json
imagegen-bridge --set runtime.default_timeout_ms=600000 config check
imagegen-bridge auth-doctor --json
imagegen-bridge auth-doctor --provider codex-app-server --json
imagegen-bridge schema --kind json-schema
imagegen-bridge schema --kind openapi
imagegen-bridge schema --kind openapi --check schemas/imagegen-bridge-v1.openapi.json
imagegen-bridge completions bash
imagegen-bridge man --output imagegen-bridge.1
```

`config check` is non-mutating. `config show` prints credential environment
variable names, never their resolved values. `config origins` prints field
paths and source keys without values. `auth-doctor` performs non-generating
provider checks.

## Output and exit contract

Primary results go to stdout. Diagnostics and server status go to stderr.
`--json` emits stable JSON; `--plain` emits compact line-oriented output;
human output never prints base64 bodies. JSON base64 output is refused when
stdout is an interactive terminal unless `--allow-inline` is explicit.

Local agents can request a separate, explicit path-bearing envelope:

```sh
imagegen-bridge --json --local-artifact-paths generate \
  --prompt "a paper fox" --output-dir agent --metadata sidecar
```

This flag is accepted only for non-dry-run `generate` and `edit` commands and
requires JSON output. The envelope contains the ordinary response under
`response` plus `artifacts[]` entries with verified canonical `path` and
optional `metadata_path`. It rejects non-artifact output, missing names,
oversized files, non-files, changed artifact checksums, and any canonical path
outside `artifacts.root`.
These host paths are intentionally CLI-only and must not be relayed through a
remote API or public log.

| Exit | Meaning |
| ---: | --- |
| `0` | Success |
| `2` | CLI syntax, invalid request, configuration, or idempotency conflict |
| `3` | Authentication, permission, or safety rejection |
| `4` | Input, artifact, or session failure |
| `5` | Rate limit or bounded-capacity overload |
| `6` | Unsupported capability, upstream, or protocol failure |
| `70` | Unexpected internal failure |
| `124` | Request deadline exceeded |
| `130` | Cancelled or interrupted operation |

stdin, request files, configuration files, multipart bodies, RPC messages, and
generated artifacts all retain their configured byte/count/time bounds.
