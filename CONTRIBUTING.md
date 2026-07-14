# Contributing

Contributions to Imagegen Bridge are welcome. Keep changes scoped, preserve the
provider-neutral request contract, and add tests for behavior that crosses a
provider, runtime, API, CLI, or SDK boundary.

## Development setup

Use the pinned Rust toolchain and locked dependencies:

```sh
cargo build --locked --workspace
```

Python SDK development uses uv:

```sh
cd sdks/python
uv sync --locked --extra test
```

TypeScript SDK development uses Bun:

```sh
cd sdks/typescript
bun install --frozen-lockfile
```

## Checks

Run the relevant focused tests while developing, then run the complete offline
matrix before opening a pull request:

```sh
cargo fmt --all --check
cargo run --locked -p imagegen-bridge-schema-gen -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --all-targets

(cd sdks/python && uv run --no-sync ruff format --check .)
(cd sdks/python && uv run --no-sync ruff check .)
(cd sdks/python && PYTHONPATH=src uv run --no-sync mypy)
(cd sdks/python && PYTHONPATH=src uv run --no-sync pytest)

(cd sdks/typescript && bun run lint)
(cd sdks/typescript && bun run typecheck)
(cd sdks/typescript && bun test)
(cd sdks/typescript && bun run build && bun run test:node)
```

The live OAuth tests are opt-in, can consume image-generation allowance, and
must not run as part of an ordinary pull request. See
[docs/testing.md](docs/testing.md) for their individual gates and the container
and browser test procedures.

## Pull requests

- Explain the user-visible behavior and compatibility impact.
- Include tests or fixtures that fail without the change.
- Regenerate and commit schemas only through the schema generator.
- Do not commit OAuth state, bearer tokens, generated images, databases, build
  directories, or private deployment details.
- Keep package versions unchanged unless the pull request is explicitly cutting
  a release.
