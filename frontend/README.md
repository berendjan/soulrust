# soulrust frontend

A **React** [Connect-Web](https://connectrpc.com/docs/web/getting-started)
client for soulrust's `soulrust.api.v1` Connect API. In production the built
bundle is embedded into and served by the Rust binary itself (the `ApiServer`
component), same-origin with the Connect API on `127.0.0.1:5031` — no separate
web server, no CORS.

The client is generated from the **same** `proto/` schema the Rust server is
built from — `protobuf-es` (`protoc-gen-es`) on this side, `buffa` on the Rust
side — so the two stay in lockstep over the Connect wire protocol.

## Build (hermetic, via Bazel)

The whole frontend is part of the Bazel graph and builds with no system
`node`/`npm` (Node comes from the `rules_js` toolchain, deps from the pinned
`pnpm-lock.yaml`):

```sh
bazel build //frontend:dist          # TS codegen + vite build -> dist/
bazel build //crates/soulrust:soulrust_bin   # embeds dist/ into the binary
```

The pipeline is three Bazel targets:

| Target | Does |
| --- | --- |
| `//frontend:gen_ts` | `buf generate` → `src/gen/**` (TS client from `//proto`) |
| `//frontend:dist` | `vite build` → the production bundle |
| `//crates/soulrust:web_assets` | embeds `dist/` into a generated Rust table (`src/web_assets_gen.rs`) served by the `ApiServer` |

## Develop (fast loop, via npm)

For hot-reload iteration you can still run Vite directly:

```sh
npm install
npm run generate          # buf generate -> src/gen/**
npm run dev               # Vite dev server; /api proxies to the Rust :5031
```

Run the Rust app (`bazel run //crates/soulrust:soulrust_bin`) alongside so the
Connect API is up. When you change `package.json`, regenerate the pnpm lock so
the Bazel build stays in sync: `npx pnpm@9 install --lockfile-only`.

## Layout

| Path | Purpose |
| --- | --- |
| `buf.gen.yaml` | protoc-gen-es codegen config (reads `../proto`) |
| `src/gen/` | generated client (gitignored; produced by Bazel or `npm run generate`) |
| `src/client.ts` | the Connect transport + `StatusService` client |
| `src/main.tsx` | React entry point |
| `src/App.tsx` | root component |
| `src/views/` | one component per service view |
| `vite.config.ts` | React plugin + dev `/api` → `:5031` proxy |
| `embed.mjs` | turns the built `dist/` into the embedded Rust asset table |

## Status

`StatusService.GetStatus` is wired end to end (React → Connect → Rust,
served embedded from the binary). Search / Transfers / Browse / Shares / Config
views follow as those services are added to `soulrust.api.v1`.
