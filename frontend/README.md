# soulrust frontend

A TypeScript [Connect-Web](https://connectrpc.com/docs/web/getting-started)
client for soulrust's `soulrust.api.v1` Connect API (served by the Rust
`ApiServer` component on `127.0.0.1:5031`).

The client is generated from the **same** `proto/` schema the Rust server is
built from — `protobuf-es` (`protoc-gen-es`) on this side, `buffa` on the Rust
side — so the two stay in lockstep over the Connect wire protocol.

## Develop

```sh
npm install
npm run generate          # buf generate -> src/gen/** (TS client from ../proto)
npm run dev               # Vite dev server; /api proxies to the Rust :5031
```

Run the Rust app (`bazel run //crates/soulrust:soulrust_bin`) alongside so the
Connect API is up.

## Build

```sh
npm run build             # tsc typecheck + vite production bundle -> dist/
```

## Layout

| Path | Purpose |
| --- | --- |
| `buf.gen.yaml` | protoc-gen-es codegen config (reads `../proto`) |
| `src/gen/` | generated client (gitignored; run `npm run generate`) |
| `src/client.ts` | the Connect transport + `StatusService` client |
| `src/main.ts` | renders session status; first of the per-service views |
| `vite.config.ts` | dev `/api` → `http://127.0.0.1:5031` proxy |

## Status

`StatusService.GetStatus` is wired end to end. Search / Transfers / Browse /
Shares / Config views follow as those services are added to `soulrust.api.v1`.
