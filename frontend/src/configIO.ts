// Shared config read/write. SetConfig replaces the whole Config, so a view that
// edits only one section must send all the others unchanged. patchConfig fetches
// the current config, lets the caller overlay its section, and saves the whole
// thing. Secrets are sent blank; the server keeps the stored value for an empty
// secret (see api_server::set_config).
import { configClient } from "./client";
import type { Config } from "./gen/soulrust/api/v1/api_pb";

export function toInit(c: Config) {
  return {
    server: {
      host: c.server?.host ?? "",
      port: c.server?.port ?? 0,
      username: c.server?.username ?? "",
      password: "",
      listenPort: c.server?.listenPort ?? 0,
    },
    spotify: { clientId: c.spotify?.clientId ?? "", clientSecret: "" },
    update: {
      enabled: c.update?.enabled ?? false,
      autoApply: c.update?.autoApply ?? false,
      repo: c.update?.repo ?? "",
    },
    ui: { bindAddr: c.ui?.bindAddr ?? "", openBrowser: c.ui?.openBrowser ?? false },
    sharing: {
      folders: c.sharing?.folders ?? [],
      downloadDir: c.sharing?.downloadDir ?? "",
      incompleteDir: c.sharing?.incompleteDir ?? "",
      uploadSlots: c.sharing?.uploadSlots ?? 0,
      fifoQueue: c.sharing?.fifoQueue ?? false,
      respondToSearches: c.sharing?.respondToSearches ?? false,
      maxSearchResults: c.sharing?.maxSearchResults ?? 0,
      minResultFiles: c.sharing?.minResultFiles ?? 0,
      minPeerUploadSpeed: c.sharing?.minPeerUploadSpeed ?? 0,
      maxPeerQueueLength: c.sharing?.maxPeerQueueLength ?? 0,
      maxDownloadSpeed: c.sharing?.maxDownloadSpeed ?? 0,
      maxUploadSpeed: c.sharing?.maxUploadSpeed ?? 0,
      organizeDownloads: c.sharing?.organizeDownloads ?? true,
    },
  };
}

export type ConfigInit = ReturnType<typeof toInit>;

// Fetch the current config, overlay the caller's edits, save. Returns the
// server error message ("" on success).
export async function patchConfig(patch: (init: ConfigInit) => void): Promise<string> {
  const current = await configClient.getConfig({});
  const init = toInit(current);
  patch(init);
  const res = await configClient.setConfig(init);
  return res.error;
}
