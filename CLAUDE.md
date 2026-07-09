# CLAUDE.md

Guidance for Claude Code when working in this repository.

## Workflow

- **Always work on the `main` branch and push directly to the remote.** Do not
  create feature branches or open pull requests — commit to `main` and push.
- **Each push is a new version, not each change.** Commits may accumulate
  locally, but every push to the remote must carry a version bump: increment
  the version (single source of truth kept in sync across
  `crates/soulrust/src/version.rs`, `crates/soulrust/Cargo.toml`, and
  `MODULE.bazel`) and push a matching `vX.Y.Z` tag alongside the commits.
  Pushing a `v*` tag triggers the release workflow
  (`.github/workflows/release.yml`), which builds the installers and the raw
  binaries the self-updater fetches.

## Build & test

- The project builds with **Bazel**, not Cargo. Bazel is the only thing that
  runs protobuf/bus codegen and bundles the React frontend, so it is the only
  build that produces a working binary.
- Build: `bazel build //crates/soulrust:soulrust_bin`
- Test:  `bazel test //crates/soulrust:soulrust_test`
- `cargo check` works as an editor/type-check convenience only. It is fed by
  `crates/soulrust-proto/generated/`, a committed copy of Bazel's codegen
  output, plus a `build.rs` that stubs out the embedded web assets. **After
  changing a `.proto`, refresh those copies from `bazel-bin/` or `cargo check`
  will type-check against a stale API.**
