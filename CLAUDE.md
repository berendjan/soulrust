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

- The project builds with **Bazel**, not Cargo — protobuf/bus codegen is
  Bazel-only (no `build.rs`, no committed generated code), so a plain
  `cargo build` cannot produce the generated types.
- **The build must stay hermetic.** No dependency on ambient system tools
  (`protoc`, `buf`, `node`/`npm`, plugin binaries, etc.). Toolchains and codegen
  plugins are provided by Bazel — pulled from source or pinned per-platform via
  `http_file` + sha256 — and codegen runs as a Bazel action compatible with
  `--incompatible_strict_action_env`. Any new toolchain (e.g. a JS/vite build for
  the frontend) must be brought in the same way, not shelled out to the host.
- Build: `bazel build //crates/soulrust:soulrust_bin`
- Test:  `bazel test //crates/soulrust:soulrust_test`
