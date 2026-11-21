# Releasing

| Piece | Tool | Config |
| ---- | ---- | ---- |
| Version, changelog, crates.io, `vX.Y.Z` tag | [release-plz](https://release-plz.dev) | `release-plz.toml`, `.github/workflows/release-plz.yml` |
| Binaries, installers, GitHub Release | [dist](https://github.com/axodotdev/cargo-dist) | `dist-workspace.toml`, `.github/workflows/release.yml` (generated) |

## Flow

1. Every push to `main` updates a release PR (label `release`) that bumps `Cargo.toml` and adds a
   `CHANGELOG.md` entry from the commits since the last release, grouped by Conventional Commits
   type (see [CONTRIBUTING.md](../CONTRIBUTING.md#pull-requests)). It opens as a draft.
2. Edit the PR before merging: tidy the changelog entry, pick the version, and mark it ready.
   While on 0.x, release-plz bumps the minor version for breaking changes (`type!:`) and the
   patch version otherwise. To pick another version, run
   `release-plz set-version lodestone-cli@0.X.Y` on the PR branch and push.
3. Merging it publishes `lodestone-cli` to crates.io (trusted publishing), pushes the tag and
   dispatches `release.yml` for it. A tag pushed with `GITHUB_TOKEN` starts no workflow, but a
   dispatch does, so no other credential is needed.
4. `release.yml`: dist builds each target, then creates the GitHub Release with the archives,
   checksums, the shell and PowerShell installers, and the version's `CHANGELOG.md` section as
   notes.

The release PR is opened with `GITHUB_TOKEN`, so CI doesn't run on it; it only changes the
version and the changelog. Pull requests run `dist plan`, which catches config errors without
building.

To build or release by hand:

```bash
gh workflow run release.yml -f tag=dry-run                  # build every target, publish nothing
gh workflow run release.yml --ref v0.1.0 -f tag=v0.1.0      # release an existing tag
```

## Targets

| Target | Runner | Embeddings |
| ---- | ---- | ---- |
| `aarch64-apple-darwin` | `macos-15` | Metal |
| `x86_64-apple-darwin` | `macos-15-intel` | Metal |
| `aarch64-unknown-linux-gnu` | `ubuntu-22.04-arm` | CPU |
| `x86_64-unknown-linux-gnu` | `ubuntu-22.04` | CPU |
| `x86_64-pc-windows-msvc` | `windows-2022` | CPU |

Binaries use the default features (no fastembed) and the `dist` profile, which is the release
profile (fat LTO, stripped). Ubuntu 22.04 keeps the glibc requirement at 2.35.
On Linux arm64, usearch is built without numkong (SIMD distance kernels), which doesn't compile
with Ubuntu 22.04's GCC and binutils; see `Cargo.toml`.

## Requirements

- **Repository setting** *Actions → General → Allow GitHub Actions to create and approve pull
  requests*, for the release PR.
- **crates.io trusted publisher** for workflow `release-plz.yml`, so no API token is stored.

## Changing the dist config

Edit `dist-workspace.toml` (or rerun `dist init`), then regenerate the workflow with the same
dist version as `cargo-dist-version`:

```bash
cargo install --locked cargo-dist@0.34.0
dist generate
dist plan        # what a release would build
```
