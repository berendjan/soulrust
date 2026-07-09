// Session status banner + activity log — the old index page's status fragment.
import { ConnectionState } from "../format";
import { useStatus } from "../status";
import type { Status } from "../gen/soulrust/api/v1/api_pb";

export function StatusPanel() {
  const status = useStatus();
  if (!status) return null;

  const [cls, text] = describe(status);
  return (
    <div className="card">
      <div className={`banner ${cls}`}>{text}</div>
      {status.log.length > 0 && <pre className="log">{[...status.log].reverse().join("\n")}</pre>}
    </div>
  );
}

function describe(status: Status): [string, string] {
  switch (status.connection) {
    case ConnectionState.LOGGED_IN:
      return ["", `Signed in as ${status.username}${status.greeting ? ` — ${status.greeting}` : ""}`];
    case ConnectionState.CONNECTING:
      return ["", "Connecting…"];
    case ConnectionState.LOGIN_FAILED:
      return ["error", `Sign-in failed: ${status.detail}`];
    default:
      return ["error", status.detail || "Not connected"];
  }
}
