// Session status view: polls StatusService.GetStatus and renders it. Same
// behaviour as the old vanilla-TS main.ts, now as a React component with the
// poll driven by an effect + interval instead of a bare setInterval.
import { useEffect, useState } from "react";
import { ConnectError } from "@connectrpc/connect";

import { statusClient } from "../client";
import type { GetStatusResponse } from "../gen/soulrust/api/v1/status_pb";

const POLL_MS = 2000;

export function StatusView() {
  const [status, setStatus] = useState<GetStatusResponse | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;

    async function refresh() {
      try {
        const next = await statusClient.getStatus({});
        if (cancelled) return;
        setStatus(next);
        setError(null);
      } catch (err) {
        if (cancelled) return;
        setError(
          err instanceof ConnectError ? err.message : String(err),
        );
      }
    }

    void refresh();
    const id = setInterval(() => void refresh(), POLL_MS);
    return () => {
      cancelled = true;
      clearInterval(id);
    };
  }, []);

  if (error) {
    return <section className="error">API error: {error}</section>;
  }
  if (!status) {
    return <section>Loading status…</section>;
  }
  if (!status.loggedIn) {
    return <section>Not connected.</section>;
  }
  return (
    <section>
      <p>
        Logged in as <b>{status.username}</b> ({status.ownIp}) —{" "}
        {status.greeting}
      </p>
      <p>Sharing {status.sharedFiles} file(s).</p>
    </section>
  );
}
