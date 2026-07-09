// Subscribe to a server-streaming `Watch*` RPC and expose the latest snapshot.
// Reconnects with a short backoff if the stream ends or errors, so a dropped
// connection (server restart, network blip) recovers on its own.
import { useEffect, useRef, useState } from "react";

export function useWatch<T>(
  subscribe: (signal: AbortSignal) => AsyncIterable<T>,
): T | null {
  const [value, setValue] = useState<T | null>(null);
  const subRef = useRef(subscribe);
  subRef.current = subscribe;

  useEffect(() => {
    const ctrl = new AbortController();
    let stopped = false;
    (async () => {
      while (!stopped) {
        try {
          for await (const msg of subRef.current(ctrl.signal)) {
            setValue(msg);
          }
        } catch {
          // fall through to reconnect
        }
        if (stopped) break;
        await new Promise((r) => setTimeout(r, 1000));
      }
    })();
    return () => {
      stopped = true;
      ctrl.abort();
    };
    // Subscribe once on mount; subRef keeps the latest closure without resubscribing.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return value;
}
