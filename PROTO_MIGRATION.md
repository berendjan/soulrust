# Re-platforming soulrust: buffa on the bus + a connectrpc API

This is the agreed plan for moving soulrust off its current `bincode`-over-bus,
`tiny_http`+htmx shape onto a protobuf wire format with a typed RPC API. It is a
**program of work**, sequenced in stages, each independently reviewable.

## Goals (decided)

1. **Whole bus → buffa protobuf.** Every bus message is defined in `.proto` and
   encoded with [`anthropics/buffa`](https://github.com/anthropics/buffa) instead
   of bincode. Read-model components decode **zero-copy `…View<'a>`** directly over
   the ring-buffer byte slice rust-messenger already hands a handler — this is the
   "messages on the bus, read as a view" model.
2. **A first-class Connect API.** A [`connectrpc`](https://github.com/anthropics/connect-rust)
   (Connect + gRPC + gRPC-Web, Tower/Hyper) service is a **public, versioned API**
   consumed by *both* the new frontend *and* external apps — not just UI glue.
3. **New TypeScript frontend** (Connect-Web) replacing `tiny_http` + htmx.
4. **Hermetic Bazel build.** No system `buf`/`protoc`. The proto toolchain and the
   `protoc-gen-buffa` / `protoc-gen-connect-rust` plugins are provided by Bazel and
   codegen runs as a Bazel action, compatible with `--incompatible_strict_action_env`.
5. **Functional payload** (the original ask): a transfer manager (download + upload,
   progress/cancel/pause/resume/retry), share management (add/remove/rescan), and
   bandwidth throttling — delivered *through* the new stack.

## The two layers (do not conflate)

- **buffa = encoding.** Replaces bincode at the `impl_bus_message!` seam
  (`crates/soulrust/src/messages.rs`). In-process. Invisible to users.
- **connectrpc = network transport.** A Tower/Hyper HTTP server, an *alternative*
  to the `tiny_http` edge. Needs a client (the TS frontend / external apps).

They are independent and staged separately below.

## Key technical risk (de-risk first)

**Hermetic buffa + connectrpc codegen under rules_rust + strict action env.**
The buffa docs' happy path (BSR remote plugin, or `buffa-build` → ambient `protoc`)
is *not* hermetic. We must instead:

- get a hermetic `protoc` (via the `protobuf` / `toolchains_protoc` Bazel module);
- get the two plugin binaries hermetically — **either** built from source as
  `rust_binary` targets through crate_universe (`protoc-gen-buffa`,
  `connectrpc-codegen`), **or** pinned per-platform release binaries via
  `http_file` + sha256;
- write a Bazel codegen rule (model it on rules_rust's `//proto/prost`) that runs
  `protoc --buffa_out --connect-rust_out` over the `.proto` set and feeds the
  generated `.rs` into a `rust_library` alongside the `buffa` / `connectrpc`
  runtime crates.

**Stage 0 is a throwaway spike proving exactly this** on one trivial `.proto`
before any app code is touched. If from-source plugins are too heavy, fall back to
pinned release binaries.

## Stages

- **Stage 0 — Hermetic codegen spike.** One `greet.proto` → buffa owned+view types
  → a connect `GreetService` → built and unit-tested entirely by `bazel test`, no
  system `buf`/`protoc`. Decides from-source-vs-pinned plugins. *Gate: passes in CI.*
- **Stage 1 — Bus seam.** Introduce `buffa`; reshape `impl_bus_message!` to encode
  via buffa (`encoded_len`/encode-into-slice) and decode owned + view. Migrate bus
  messages package-by-package (`soulrust.bus.v1`), keeping bincode for un-migrated
  ones during the transition. Re-validate the ring-size budget in `COMPARISON.md`.
- **Stage 2 — Read-as-view.** Convert the read-model components (`Ui`, `Browse`,
  new `Transfers`) to decode `…View<'a>` over the ring slice.
- **Stage 3 — Connect service.** Define `soulrust.api.v1` (Search, Transfers,
  Browse, Shares, Config) with server-streaming for live transfer/search updates.
  Stand up the connectrpc Tower server on a tokio thread; bridge it to the bus.
- **Stage 4 — Functional payload.** Transfer manager, share management, throttling —
  built natively on the new messages + RPCs.
- **Stage 5 — TS frontend.** Connect-Web client; retire `tiny_http` + htmx.

## Open items to confirm as we hit them

- Plugin provisioning: from-source (crate_universe) vs pinned release binaries.
- Whether `tiny_http`/htmx is deleted at Stage 5 or kept briefly behind the API.
- gRPC-Web vs Connect protocol for the browser (connectrpc serves all three).
