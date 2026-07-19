# Releasing Imagegen Bridge

Imagegen Bridge uses one version across the Rust workspace, Python SDK, and
TypeScript SDK. A `vMAJOR.MINOR.PATCH` tag starts the release workflow, which
tests the tagged source and creates a GitHub Release containing:

- Linux x86-64 and ARM64 CLI archives;
- macOS Apple Silicon and Intel CLI archives;
- a Windows x86-64 CLI archive;
- a `SHA256SUMS` file.

Release notes require no hand-written Markdown. The tagged workflow finds the
previous stable SemVer tag, asks GitHub for categorized pull-request notes,
reads the first-parent commit history, and generates a temporary
`release-notes.md`. The resulting release always contains automatic highlights,
Conventional Commit categories, linked commits, contributors, the full compare
link, downloads, and package destinations. If GitHub cannot generate PR notes,
the local commit-based sections still make the release complete.

Publishing that GitHub Release starts the package workflow. Its independent
jobs publish the Rust workspace to crates.io, the Python SDK to PyPI, the
TypeScript SDK to npm, and a multi-architecture container to GHCR. A registry
failure does not remove a successfully created GitHub Release.

## One-time registry setup

Configure these GitHub environments before the first tag:

| Environment | Registry configuration |
| --- | --- |
| `crates-io` | Bootstrap the first publication with a short-lived crates.io token, then use Trusted Publishing as described below |
| `pypi` | Create a pending PyPI trusted publisher for owner `Crimsab`, repository `imagegen-bridge`, workflow `publish.yml`, environment `pypi` |
| `npm` | Bootstrap the unscoped `imagegen-bridge` package with a granular `NPM_TOKEN`; then configure its GitHub trusted publisher for workflow `publish.yml`, environment `npm`, and remove the long-lived token |

GHCR uses the workflow's short-lived `GITHUB_TOKEN`; no repository secret is
needed. GitHub initially creates a container package as private, so make
`ghcr.io/crimsab/imagegen-bridge` public in the package settings after its first
publication.

The npm workflow uses Node 22.14 and npm 11.5.1 or newer for OIDC trusted
publishing. Project dependencies, tests, and builds continue to use Bun. PyPI
publishing is OIDC-only and does not require a long-lived API token.

### Bootstrap crates.io, then remove the token

crates.io cannot configure a trusted publisher until a crate has been published
once. Sign in to crates.io with GitHub, verify the account email, and create an
expiring API token that permits publishing new crates. Store it only as the
`CARGO_REGISTRY_TOKEN` secret in the GitHub `crates-io` environment. Do not run
`cargo login` on a workstation or server for the automated release path.

The first package workflow tries OIDC and falls back to that bootstrap secret.
After all nine Rust packages have been published, add a GitHub trusted publisher
to each package with these values:

- repository owner: `Crimsab`;
- repository: `imagegen-bridge`;
- workflow: `publish.yml`;
- environment: `crates-io`.

Delete `CARGO_REGISTRY_TOKEN` after the trusted publishers are configured. Future
releases receive a short-lived token from crates.io through GitHub OIDC, and the
workflow revokes it automatically when the job ends. Enabling crates.io's
Trusted Publishing Only mode after verification also prevents traditional API
tokens from publishing later versions.

## Cut a release

1. Update the shared workspace version in `Cargo.toml`, the Python version in
   `sdks/python/pyproject.toml`, and the TypeScript version in
   `sdks/typescript/package.json`.
2. Refresh `Cargo.lock` and `sdks/typescript/bun.lock` when required.
3. Run the complete test matrix documented in [testing.md](testing.md).
4. Commit and push the release commit.
5. Create and push an annotated version tag:

   ```sh
   git tag -a v0.2.0 -m "Imagegen Bridge v0.2.0"
   git push origin v0.2.0
   ```

The workflows reject a tag whose version does not match every package. Never
reuse or move a published version tag; create a new patch release instead.

Use Conventional Commit subjects so automatic categories remain meaningful:
`feat:` for compatible functionality, `fix:` for corrections, `perf:` for
performance, and `docs:`, `test:`, `refactor:`, `ci:`, `build:`, or `chore:`
for their corresponding maintenance areas. Add `!` or a `BREAKING CHANGE:`
footer when a public contract is intentionally incompatible. Unrecognized
subjects remain visible under `Other changes`; they never disappear.

The standalone updater trusts only the immutable assets of the latest GitHub
Release and requires an exact `SHA256SUMS` entry before extraction. Keep archive
names and checksum generation in the release workflow stable, or treat any
intentional naming change as an updater compatibility change.
