# cbox release CI: building and distributing prebuilt binaries

## Why this exists

`cbox` (`cbox/`) is a Rust binary that every `just up`/`exec`/`down`/`list` recipe shells out to
(`cbox_bin` in `justfile`). Building it locally (`just build-cbox` → `cd cbox && cargo build
--release`) requires a Rust toolchain and `protoc >= 3.12` on the contributor's machine, because
`boxlite`'s `build.rs` compiles `boxlite-shared` from source (`docs/design/cbox.md:60-61`).

This spec adds a GitHub Actions pipeline that builds and tests `cbox` on Linux and macOS on every
push, and publishes prebuilt binaries as GitHub Release assets so someone who just wants to run
the box tooling — not hack on `cbox`'s Rust source — can fetch a binary instead of compiling one.

**Explicitly not in scope:** a Docker-based local build path. That idea was explored and dropped
— Docker can only ever produce Linux binaries (a container is a Linux kernel/filesystem
regardless of host OS), so it can't help macOS contributors at all, and `just build-cbox`/
`up-dev` need to keep compiling locally for anyone actively editing `cbox`'s source, since a CI
artifact can never reflect uncommitted local changes. This spec is purely additive: `build-cbox`
is untouched.

## Matrix scope

Two targets, matching GitHub-hosted runners with zero extra cross-compile setup:

| OS | Runner | Asset name |
| --- | --- | --- |
| Linux x86_64 | `ubuntu-latest` | `cbox-linux-x86_64` |
| macOS arm64 (Apple Silicon) | `macos-14` | `cbox-macos-arm64` |

Linux arm64 and Intel macOS are out of scope for this iteration — add them later as more matrix
rows if needed; nothing below depends on there being exactly two.

The Linux binary is dynamically linked against whatever glibc `ubuntu-latest` ships at build time
(currently Ubuntu 24.04 → glibc 2.39); it is not guaranteed to run on older distros (Ubuntu 22.04,
Debian 12, etc.), and this floor rises silently whenever GitHub repoints `ubuntu-latest`. Pin to a
specific older runner (e.g. `ubuntu-22.04`) in a future iteration if a lower floor is needed.

## Workflow: `.github/workflows/cbox-release.yml`

Two jobs.

### `build` (runs on every push, any branch, and on `v*.*.*` tags)

Matrix over the table above. Steps:

1. Checkout.
2. Install `protoc`: `apt-get install -y protobuf-compiler` (Linux) / `brew install protobuf`
   (macOS).
3. No toolchain-install action needed — confirmed against `actions/runner-images`' own readmes
   that both `ubuntu-latest` (Ubuntu 24.04: "Cargo 1.97.1, Rust 1.97.1, Rustup 1.29.0") and
   `macos-14` ("Rust 1.96.0", "Rustup 1.29.0") ship Rust/Cargo/rustup preinstalled. A plain
   `rustup update stable` run step is enough to land on a current toolchain, with no third-party
   action to vet or pin (see Action pinning, below).
4. `cd cbox && cargo test --release` — runs the existing suite (`cbox/tests/`) on both platforms.
   This is the CI-validation half: a `cbox` change that breaks Linux or macOS fails here on every
   push, tag or not.
5. `cargo build --release`.
6. **No code-signing step.** Confirmed empirically: a local `cargo build --release` on Apple
   Silicon needed no explicit `codesign` call to make `boxlite`'s macOS backend work — the
   linker's default ad-hoc signature (applied to every binary on Apple Silicon as a precondition
   for the OS to load it at all) is sufficient. CI should therefore produce a working binary with
   nothing extra. If a future macOS version or a different Mac configuration turns out to need
   explicit entitlements after all, that's a follow-up to make once observed, not something to
   speculatively build in now — this was flagged and dropped from an earlier draft of this design
   for exactly that reason.
7. Rename the binary to its asset name (table above) and upload via `actions/upload-artifact`
   (default retention). This happens on every push regardless of branch or tag — it's what gives
   every push an inspectable, downloadable binary even without a release.

### `release` (needs: `build`)

Reads `cbox/Cargo.toml`'s `version` field (currently `0.1.0`) to compute a tag, and behaves
differently depending on what triggered the run:

- **Push to `dev`:** downloads both artifacts (`actions/download-artifact`), publishes/overwrites a rolling release tagged
  `v<version>-dev` (e.g. `v0.1.0-dev`) — delete the existing release+tag first if present (`gh
  release delete v0.1.0-dev --yes --cleanup-tag || true`), then `gh release create v0.1.0-dev
  cbox-linux-x86_64 cbox-macos-arm64 --title "cbox dev build" --prerelease`. This is the "latest
  from dev" binary — always current, always overwritten, marked as a prerelease so it doesn't
  read as a real version.
- **Push of a tag matching `v*.*.*`:** downloads both artifacts, publishes a real numbered release
  at that tag: `gh release create <tag> cbox-linux-x86_64 cbox-macos-arm64 --generate-notes`.
  Cutting a real release is a manual act — bump `cbox/Cargo.toml`'s version, `git tag vX.Y.Z`,
  `git push origin vX.Y.Z`.
- **Any other branch push:** job is skipped (`if:` guard) — build+test still ran, but nothing is
  published. Feature-branch CI runs stay build/test-only, matching how a contributor working on
  `cbox` would want fast feedback without every branch cluttering the release list.

Uses the `gh` CLI (preinstalled on GitHub-hosted runners, authenticated via the default
`GITHUB_TOKEN`) rather than a third-party release action — one fewer external action to trust for
something this workflow-shaped. The job needs `permissions: contents: write` to create releases.

## Action pinning & supply chain

Only actions published by a GitHub-verified creator are used, and every `uses:` is limited to
three: `actions/checkout`, `actions/upload-artifact`, `actions/download-artifact` — all under the
`actions` org (GitHub's own, carries the Marketplace "Verified creator" badge). No Rust-toolchain
action is needed at all (see step 3 above), which removes what would otherwise be the one
plausible non-`actions`-org dependency (e.g. `dtolnay/rust-toolchain`, well-regarded but not
itself a verified-creator publisher). Release publishing goes through the `gh` CLI directly
rather than a marketplace release action, for the same reason.

Each `uses:` is pinned to its full 40-character commit SHA, with the version it corresponds to
as a trailing comment — the standard mitigation against a tag being retargeted after the fact:

```yaml
- uses: actions/checkout@11bd71901bbe5b1630ceea73d27597364c9af683 # v4.2.2
- uses: actions/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02 # v4.6.2
- uses: actions/download-artifact@fa0a91b85d4f404e444e00e005971372dc801d16 # v4.1.8
```

The exact SHAs above are illustrative — resolve the real ones at implementation time (e.g. `gh
api repos/actions/checkout/git/refs/tags/v4.2.2`, or copying the commit SHA from the tag's page
on GitHub) rather than trusting a value typed from memory into this doc.

### Keeping pins current: `.github/dependabot.yml`

```yaml
version: 2
updates:
  - package-ecosystem: "github-actions"
    directory: "/"
    schedule:
      interval: "weekly"
```

Dependabot natively understands the SHA-pin-plus-version-comment convention: it resolves the new
release's commit SHA and updates both the hash and the trailing comment in the same PR, so pinning
to a hash doesn't turn into a manual chore — it still gets bumped automatically, just as a
reviewable PR instead of a silent floating-tag update. This file has no dependency on the
workflow existing yet; it's safe to add now and will simply have nothing to bump until
`cbox-release.yml` lands.

## Local install path: `just install-cbox [tag]`

A new justfile recipe, additive alongside the existing `build-cbox`:

- `tag` defaults to `v<cbox/Cargo.toml version>-dev` (the rolling dev release).
- Detects host OS/arch via `uname -s`/`uname -m` to pick the asset name (`cbox-linux-x86_64` /
  `cbox-macos-arm64`); errors clearly on any other OS/arch rather than silently doing nothing.
- `curl -fsSL "https://github.com/the-mentor/claude-boxlite/releases/download/<tag>/<asset>" -o
  cbox/target/release/cbox && chmod +x cbox/target/release/cbox`. The repo is public, so this
  needs no auth token.
- Writes to the exact path `cbox_bin` (`justfile:257`) already expects — `up`/`exec`/`down`/
  `list` need no changes to consume a binary fetched this way instead of compiled locally.

`build-cbox` is untouched: it still runs `cd cbox && cargo build --release` for anyone actively
developing `cbox`'s Rust source, where a CI-published binary would be stale by definition.

## Error handling / edge cases

- **Rolling-release overwrite race:** if `dev` is pushed to twice in quick succession, both
  `release` jobs could attempt to delete+recreate `v0.1.0-dev` concurrently. GitHub Actions
  doesn't serialize workflow runs by default; add `concurrency: group: cbox-dev-release` (no
  `cancel-in-progress`, so both runs still complete on the queue rather than being killed
  mid-`gh` command) to the `release` job to avoid a delete-created-by-the-other-run race.
- **`cbox/Cargo.toml` version not bumped between dev pushes:** harmless — the rolling release tag
  stays `v0.1.0-dev` until someone bumps the version; it's expected to move only when a real
  release is being prepared, not on every dev push.
- **A push to `dev` whose `cargo test` fails:** `release` has `needs: build`, so it doesn't run —
  no broken binary gets published. The previous good `dev` release stays live until a passing
  push comes through.
- **`install-cbox` on an unreleased tag:** `curl -f` fails loudly (404) rather than silently
  writing an empty/HTML error page to `cbox/target/release/cbox`; check the write is non-empty or
  rely on `curl -f`'s exit code before `chmod +x`.

## Open questions

- macOS codesigning has only been verified against one Apple Silicon machine (see step 6 above).
  If a released binary fails to invoke `boxlite`'s macOS backend on some other Mac, that's the
  first place to look — revisit with an explicit `codesign --sign - --entitlements ...` step at
  that point, not before.
- No Linux arm64 or Intel macOS row yet; add as its own matrix entry later if a contributor
  actually needs one — nothing in this design assumes a fixed matrix size.
