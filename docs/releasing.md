# Releasing Imagegen Bridge

Imagegen Bridge uses one version across the Rust workspace, Python SDK, and
TypeScript SDK. A `vMAJOR.MINOR.PATCH` tag starts the release workflow, which
tests the tagged source and creates a GitHub Release containing:

- Linux x86-64 and ARM64 CLI archives;
- macOS Apple Silicon and Intel CLI archives;
- a Windows x86-64 CLI archive;
- a `SHA256SUMS` file.

Publishing that GitHub Release starts the package workflow. Its independent
jobs publish the Rust workspace to crates.io, the Python SDK to PyPI, the
TypeScript SDK to npm, and a multi-architecture container to GHCR. A registry
failure does not remove a successfully created GitHub Release.

## One-time registry setup

Configure these GitHub environments before the first tag:

| Environment | Registry configuration |
| --- | --- |
| `crates-io` | Add a repository secret named `CARGO_REGISTRY_TOKEN` containing a crates.io token scoped for publishing the Imagegen Bridge crates |
| `pypi` | Create a pending PyPI trusted publisher for owner `Crimsab`, repository `imagegen-bridge`, workflow `publish.yml`, environment `pypi` |
| `npm` | Bootstrap the unscoped `imagegen-bridge` package with a granular `NPM_TOKEN`; then configure its GitHub trusted publisher for workflow `publish.yml`, environment `npm`, and remove the long-lived token |

GHCR uses the workflow's short-lived `GITHUB_TOKEN`; no repository secret is
needed. GitHub initially creates a container package as private, so make
`ghcr.io/crimsab/imagegen-bridge` public in the package settings after its first
publication.

The npm workflow uses Node 22.14 and npm 11.5.1 or newer for OIDC trusted
publishing. Project dependencies, tests, and builds continue to use Bun. PyPI
publishing is OIDC-only and does not require a long-lived API token.

## Cut a release

1. Update the shared workspace version in `Cargo.toml`, the Python version in
   `sdks/python/pyproject.toml`, and the TypeScript version in
   `sdks/typescript/package.json`.
2. Refresh `Cargo.lock` and `sdks/typescript/bun.lock` when required.
3. Run the complete test matrix documented in [testing.md](testing.md).
4. Commit and push the release commit.
5. Create and push an annotated version tag:

   ```sh
   git tag -a v0.1.0 -m "Imagegen Bridge v0.1.0"
   git push origin v0.1.0
   ```

The workflows reject a tag whose version does not match every package. Never
reuse or move a published version tag; create a new patch release instead.
