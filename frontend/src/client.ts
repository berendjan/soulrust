// Connect clients for soulrust.api.v1, talking to the Rust connectrpc server
// (crates/soulrust/src/components/api_server.rs). In dev the Vite server proxies
// /api → :5031; in production the SPA is served same-origin by the binary.
import { createClient } from "@connectrpc/connect";
import { createConnectTransport } from "@connectrpc/connect-web";

import {
  BrowseService,
  ConfigService,
  SearchService,
  StatusService,
  SystemService,
  TransfersService,
  UpdaterService,
} from "./gen/soulrust/api/v1/api_pb";

const transport = createConnectTransport({
  baseUrl: import.meta.env.DEV ? "/api" : "/",
});

export const statusClient = createClient(StatusService, transport);
export const searchClient = createClient(SearchService, transport);
export const transfersClient = createClient(TransfersService, transport);
export const browseClient = createClient(BrowseService, transport);
export const configClient = createClient(ConfigService, transport);
export const updaterClient = createClient(UpdaterService, transport);
export const systemClient = createClient(SystemService, transport);
