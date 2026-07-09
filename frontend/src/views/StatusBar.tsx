// Live connection status + recent activity log, streamed from StatusService.
import { statusClient } from "../client";
import { useWatch } from "../useWatch";
import { ConnectionState } from "../format";
import type { Status } from "../gen/soulrust/api/v1/api_pb";

export function StatusBar() {
  const status = useWatch<Status>((signal) => statusClient.watchStatus({}, { signal }));

  const conn = status?.connection ?? ConnectionState.UNSPECIFIED;
  const [pill, text] = describe(status, conn);

  return (
    <div className="statusbar">
      <span className={`pill ${pill}`}>● {text}</span>
      {status && status.log.length > 0 && (
        <span className="statusbar-log" title={status.log.slice(-8).join("\n")}>
          {status.log[status.log.length - 1]}
        </span>
      )}
    </div>
  );
}

function describe(status: Status | null, conn: number): [string, string] {
  if (!status) return ["", "connecting…"];
  switch (conn) {
    case ConnectionState.LOGGED_IN:
      return ["ok", `signed in as ${status.username}`];
    case ConnectionState.CONNECTING:
      return ["", "connecting…"];
    case ConnectionState.LOGIN_FAILED:
      return ["warn", `sign-in failed: ${status.detail}`];
    default:
      return ["warn", status.detail || "not connected"];
  }
}
