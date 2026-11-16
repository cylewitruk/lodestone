# Releasing

| Piece | Tool | Config |
| ---- | ---- | ---- |
| Version, changelog, crates.io, `vX.Y.Z` tag | [release-plz](https://release-plz.dev) | `release-plz.toml`, `.github/workflows/release-plz.yml` |
| Binaries, installers, GitHub Release | [dist](https://github.com/axodotdev/cargo-dist) | `dist-workspace.toml`, `.github/workflows/release.yml` (generated) |

## Flow

1. Every push to `main` updates a release PR (label `release`) that bumps `Cargo.toml` and adds a
   `CHANGELOG.md` entry from the commits since the last release.
2. Edit the PR before merging: tidy the changelog entry, and pick the version. release-plz bumps
   the patch version for `area: summary` commits; for a minor bump, run
   `release-plz set-version lodestone-cli@0.X.0` on the PR branch and push.
3. Merging it publishes `lodestone-cli` to crates.io (trusted publishing) and pushes the tag.
4. The tag starts `release.yml`: dist builds each target, then creates the GitHub Release with the
   archives, checksums, the shell and PowerShell installers, and the version's `CHANGELOG.md`
   section as notes.

Pull requests run `dist plan` only, which catches config errors without building.

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

## One-time setup

1. **GitHub App** (tags pushed with `GITHUB_TOKEN` start no workflows, so release-plz needs its own
   token): create an App under your account settings with repository permissions *Contents* and
   *Pull requests* set to read and write, no webhook, and install it on this repository. Add
   repository secrets `RELEASE_APP_ID` and `RELEASE_APP_PRIVATE_KEY` (a generated private key).
2. **First release by hand**, since crates.io trusted publishing needs the crate to exist. From an
   up-to-date `main`:

   ```bash
   cargo publish --locked
   git tag v0.1.0 && git push origin v0.1.0
   ```

   The tag builds and publishes the v0.1.0 GitHub Release.
3. **Trusted publisher:** on crates.io, under the crate's settings, add GitHub repository
   `cylewitruk/lodestone` with workflow `release-plz.yml`.

## Changing the dist config

Edit `dist-workspace.toml` (or rerun `dist init`), then regenerate the workflow with the same
dist version as `cargo-dist-version`:

```bash
cargo install --locked cargo-dist@0.34.0
dist generate
dist plan        # what a release would build
```

## Package managers

| Channel | Status | What it takes |
| ---- | ---- | ---- |
| Shell / PowerShell installer | done | dist |
| crates.io (`cargo install`) | done | release-plz |
| cargo-binstall | should work | dist's archive names follow binstall's defaults; check after the first release |
| Homebrew | ready to enable | create `cylewitruk/homebrew-tap`, add a `HOMEBREW_TAP_TOKEN` secret (PAT with write access to the tap), then in `dist-workspace.toml` add `"homebrew"` to `installers`, `tap = "cylewitruk/homebrew-tap"` and `publish-jobs = ["homebrew"]` |
| winget | not built in to dist | a custom publish job (`publish-jobs = ["./publish-winget"]`, a reusable workflow) running `komac` or `winget-releaser` against the Windows zip; the first manifest is a manual PR to `microsoft/winget-pkgs` |
| Scoop | not built in to dist | a `scoop-bucket` repository with a manifest whose `autoupdate` points at the Windows zip |
| Chocolatey | not built in to dist | a `.nuspec` package wrapping the Windows zip, pushed with an API key from a custom publish job; new packages go through moderation |
| apt / `.deb` | not built in to dist | `cargo-deb` to build `.deb` files attached to the release; `apt install` additionally needs a signed, hosted repository |
| npm | available | dist's `npm` installer, if a `npx` entry point is ever useful |

Custom publish jobs run after the GitHub Release exists, so each can download its archive and
checksum from the release.
