# Test matrix

The default test suite is deterministic and does not read Codex OAuth state,
contact an image provider, or generate paid/subscription-backed images.

## Offline gates

Run the Rust contract, lint, schema, process, and integration suite:

```sh
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --all-targets
cargo doc --locked --workspace --no-deps
cargo run --locked -p imagegen-bridge-schema-gen -- --check
```

The Python and TypeScript suites both start the shared Rust fixture server. It
also serves the production dashboard assets without contacting Codex.

```sh
cargo build --locked -p imagegen-bridge-sdk-mock-server

cd sdks/python
uv sync --locked --extra test --no-install-project
PYTHONPATH=src uv run --no-sync ruff format --check .
PYTHONPATH=src uv run --no-sync ruff check .
PYTHONPATH=src uv run --no-sync mypy
PYTHONPATH=src uv run --no-sync pytest
uv build --build-constraints build-constraints.txt --require-hashes

cd ../typescript
bun install --frozen-lockfile
bun run lint
bun run typecheck
bun run build
bun test
bun run test:node
bun pm pack --dry-run
```

`tests/container-smoke.sh` builds a non-root, read-only image around a fake
Codex JSONL process. It verifies authenticated API access, cached readiness,
graceful shutdown, SQLite persistence, and app-server thread resume after a
real container stop/start.

## Codex OAuth gates

Live tests are ignored unless their individual environment gate is set. They
require `codex login` and can consume the account's image-generation allowance.
Run only the gate being evaluated.

| Gate | External work | Contract verified |
| --- | --- | --- |
| `IMAGEGEN_BRIDGE_LIVE_BOOTSTRAP=1` | No generation | Both Codex OAuth adapters initialize and report ready |
| `IMAGEGEN_BRIDGE_LIVE_CODEX=1` | Two app-server images | Verified bytes, `generate` to `auto` action negotiation, plus persistent session reuse on the same thread |
| `IMAGEGEN_BRIDGE_LIVE_CODEX_RESPONSES=1` | One low-quality Responses image | Reference input, explicit size/background, high input fidelity, edit action, partial-image request, actual dimensions/checksum, and verified final bytes |

```sh
IMAGEGEN_BRIDGE_LIVE_BOOTSTRAP=1 \
  cargo test --locked -p imagegen-bridge --features codex-responses \
  live_config_bootstrap_reports_both_codex_providers_ready -- --ignored

IMAGEGEN_BRIDGE_LIVE_CODEX=1 \
  cargo test --locked -p imagegen-bridge-codex-app-server \
  live_codex_generates_a_verified_image -- --ignored

IMAGEGEN_BRIDGE_LIVE_CODEX_RESPONSES=1 \
  cargo test --locked -p imagegen-bridge-codex-responses \
  live_codex_responses_generates_a_verified_image -- --ignored
```

`imagegen-bridge doctor --live-probe` is the CLI-level single-image gate. It
uses metadata-only delivery, reports duration/dimensions/format, and still
performs a real generation. Setup never invokes it unless `--live-probe` is
explicitly present and separately confirmed.

The reserved official OpenAI provider is not implemented, so there is no API
key live gate. Adding one must not reuse any Codex OAuth gate.

## Browser dashboard

Start the fixture server and open the printed URL with `/dashboard` appended:

```sh
cargo run --locked -p imagegen-bridge-sdk-mock-server
```

Use bearer token `sdk-test-token`. Browser QA should cover connection state,
job submission, details, favorite/hide/restore, filters, operations diagnostics,
the capability matrix, and a narrow mobile viewport. Reload once with console
and network diagnostics enabled; warnings, uncaught exceptions, failed asset
loads, and horizontal overflow are failures.
