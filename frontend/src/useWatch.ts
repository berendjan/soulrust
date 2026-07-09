// Subscribe to a server-streaming `Watch*` RPC and expose the latest snapshot.
// Reconnects if the stream ends or errors, so a dropped connection (server
// restart, network blip) recovers on its own. The delay backs off from 250ms to
// 10s with jitter — a server that keeps failing gets a handful of retries, not
// one per second per mounted view forever — and resets once a snapshot arrives.
import { useEffect, useRef, useState } from "react";

const MIN_DELAY = 250;
const MAX_DELAY = 10_000;

export function useWatch<T>(
  subscribe: (signal: AbortSignal) => AsyncIterable<T>,
): T | null {
  const [value, setValue] = useState<T | null>(null);
  const subRef = useRef(subscribe);
  subRef.current = subscribe;

  useEffect(() => {
    const ctrl = new AbortController();
    let stopped = false;
    let delay = MIN_DELAY;
    (async () => {
      while (!stopped) {
        try {
          for await (const msg of subRef.current(ctrl.signal)) {
            delay = MIN_DELAY;
            setValue(msg);
          }
        } catch {
          // fall through to reconnect
        }
        if (stopped) break;
        await new Promise((r) => setTimeout(r, delay * (0.5 + Math.random())));
        delay = Math.min(delay * 2, MAX_DELAY);
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
