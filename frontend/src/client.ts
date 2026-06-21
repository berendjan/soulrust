// The Connect client for soulrust.api.v1, talking to the Rust connectrpc server
// (crates/soulrust/src/components/api_server.rs) over Connect-Web.
import { createClient } from "@connectrpc/connect";
import { createConnectTransport } from "@connectrpc/connect-web";

import { StatusService } from "./gen/soulrust/api/v1/status_pb";

// The dev server proxies /api → the Rust Connect server (see vite.config.ts);
// in production the same-origin base URL is used.
const transport = createConnectTransport({
  baseUrl: import.meta.env.DEV ? "/api" : "/",
});

export const statusClient = createClient(StatusService, transport);
