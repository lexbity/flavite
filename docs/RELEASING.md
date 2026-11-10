# Releasing Flavite

Releases are built by [GoReleaser](https://goreleaser.com) (OSS) using its
`rust` builder (`cargo zigbuild` for cross-compilation). Configuration lives
in `.goreleaser.yaml`; CI lives in `.github/workflows/release.yml`; local
entry points are the `just release-*` recipes (CI runs them verbatim).

## What a release produces

For each `v*` tag, on Linux x86_64 + aarch64 (glibc; macOS/Windows are
deferred until the app supports them):

| Artifact | Notes |
|---|---|
| `flavite_<ver>_<os>_<arch>.tar.gz` | All three binaries (`flavite`, `flavite-cli`, `flavite-text-search-worker`) + README/LICENSE. |
| `flavite_<ver>_<arch>.deb` | Same binaries into `/usr/bin`, plus `flavite.desktop` and icons. |
| `checksums.txt` | SHA-256 of all artifacts. |
| AUR source `PKGBUILD` | Generated into the release; pushed to the AUR manually (see below). |

All three binaries must stay in the same directory: the regex search worker
is resolved as a sibling of the running executable
(`crates/flavite-vault/src/text_worker.rs`), never via `PATH`.

The version comes from the git tag (`v0.1.0`), matching
`workspace.package.version` in `Cargo.toml`.

## How to cut a release

```sh
git tag v0.2.0
git push origin v0.2.0     # triggers .github/workflows/release.yml
```

The `release` job (tag pushes only) runs `just release-check` then
`just release` with `GITHUB_TOKEN` (`contents: write`, job-level only) and
creates the GitHub Release on `github.com/lexbity/flavite`.

Every pull request runs the `release-snapshot` job: `goreleaser check` plus a
full `--snapshot --clean` pipeline (both targets, tarballs, `.deb`, PKGBUILD
into `dist/`) with nothing published — packaging and cross-build regressions
fail on the PR, not at tag time.

## AUR

`aur_sources` in `.goreleaser.yaml` generates a **source** PKGBUILD (builds
with `cargo build --release --locked`, the AUR-idiomatic form for the package
name `flavite`; a binary package would be `flavite-bin`).

For now `skip_upload: true`: the PKGBUILD lands in `dist/` and is pushed to
`aur.archlinux.org/flavite.git` manually. To automate later:

1. Create the `flavite` package on the AUR once, and add a passwordless SSH
   deploy key as the repo secret `AUR_KEY`.
2. Set `skip_upload: false` in `.goreleaser.yaml` and add
   `disable: "{{ .IsSnapshot }}"` to the `aur_sources` entry so snapshot runs
   never push.

## Local validation

Prerequisites (pinned the same way as `release.yml`):

```sh
# goreleaser v2.20.0, sha256-verified download
curl -sSfLO "https://github.com/goreleaser/goreleaser/releases/download/v2.20.0/goreleaser_Linux_x86_64.tar.gz"
# verify sha256 d2483159e6007f814132cb56635ad30a70013dbeb813d6033a7e01adf6ae108c, then install to PATH

# zig 0.17.1, sha256-verified (sha256 3581863e...e9b17a, see release.yml)
curl -sSfLO "https://ziglang.org/download/0.17.1/zig-x86_64-linux-0.17.1.tar.xz"

cargo install --locked cargo-zigbuild --version 0.24.1
rustup target add x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu
```

Then:

```sh
just release-check       # validate .goreleaser.yaml
just release-snapshot    # full pipeline into dist/, nothing published
```

Inspect the results before tagging, e.g.:

```sh
dpkg-deb -c dist/*.deb          # package contents
dpkg-deb -I dist/*.deb          # control fields (deps, arch)
cat dist/aur_sources/flavite/PKGBUILD   # generated AUR source package
```

## Packaging notes

- `.deb` runtime dependencies are declared in the `nfpms` overrides: winit
  dlopens wayland/xkbcommon and wgpu dlopens vulkan/EGL at runtime, so those
  system libraries are needed on the target machine but not at link time
  (which is also what makes `cargo zigbuild` cross builds work).
- Desktop entry and icons live in `packaging/` and are referenced by both the
  `.deb` contents and the AUR `package()` function. The icon is
  `packaging/icons/flavite.svg` (edit this; `flavite.png` is rendered from it
  with `rsvg-convert -w 256 -h 256`).
- New platforms: add targets to `builds[].targets` in `.goreleaser.yaml` and,
  when supported, extend `nfpms.formats` (e.g. `rpm`, `archlinux` binary
  pkg) and the matching workflow runner matrix.
