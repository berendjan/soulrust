// One session-status stream for the whole app. The sidebar chip and the Search
// page's status panel both read it, so it lives in a context rather than each
// component opening its own WatchStatus stream.
import { createContext, useContext } from "react";

import { statusClient } from "./client";
import { useWatch } from "./useWatch";
import type { Status } from "./gen/soulrust/api/v1/api_pb";

const StatusContext = createContext<Status | null>(null);

export function StatusProvider({ children }: { children: React.ReactNode }) {
  const status = useWatch<Status>((signal) => statusClient.watchStatus({}, { signal }));
  return <StatusContext.Provider value={status}>{children}</StatusContext.Provider>;
}

export function useStatus(): Status | null {
  return useContext(StatusContext);
}
