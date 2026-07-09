// Transfers: live downloads + uploads with progress, cancel/pause, and preview.
import { transfersClient } from "../client";
import { useWatch } from "../useWatch";
import {
  DownloadStatus,
  downloadStatusLabel,
  humanSize,
  isAudio,
  percent,
  UploadStatus,
} from "../format";
import { usePlayer } from "../player";
import type { Transfers } from "../gen/soulrust/api/v1/api_pb";

export function TransfersView() {
  const transfers = useWatch<Transfers>((signal) => transfersClient.watchTransfers({}, { signal }));
  const play = usePlayer();

  const downloads = transfers?.downloads ?? [];
  const uploads = transfers?.uploads ?? [];
  const active = (s: number) =>
    s === DownloadStatus.QUEUED || s === DownloadStatus.POSITION || s === DownloadStatus.STARTING;

  return (
    <div className="view">
      <h2>Downloads</h2>
      {downloads.length === 0 && <p className="muted">No downloads.</p>}
      {downloads.length > 0 && (
        <table>
          <thead>
            <tr>
              <th>File</th>
              <th>User</th>
              <th>State</th>
              <th>Progress</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {downloads.map((d, i) => (
              <tr key={`${d.username}-${d.filename}-${i}`}>
                <td className="file">{d.filename.replace(/^.*[\\/]/, "") || "(unknown)"}</td>
                <td>{d.username || "—"}</td>
                <td>
                  {downloadStatusLabel(d.status, d.place)}
                  {d.status === DownloadStatus.FAILED && d.error ? `: ${d.error}` : ""}
                </td>
                <td>
                  {d.size > 0n && (
                    <div className="bar">
                      <div className="bar-fill" style={{ width: `${percent(d.bytes, d.size)}%` }} />
                    </div>
                  )}
                  <span className="muted">
                    {d.size > 0n ? `${humanSize(d.bytes)} / ${humanSize(d.size)}` : ""}
                  </span>
                </td>
                <td className="actions">
                  {active(d.status) && (
                    <>
                      <button
                        className="xs"
                        onClick={() =>
                          transfersClient
                            .pauseDownload({ username: d.username, filename: d.filename })
                            .catch(() => {})
                        }
                      >
                        Pause
                      </button>
                      <button
                        className="xs ghost"
                        onClick={() =>
                          transfersClient
                            .cancelDownload({ username: d.username, filename: d.filename })
                            .catch(() => {})
                        }
                      >
                        Cancel
                      </button>
                    </>
                  )}
                  {d.status === DownloadStatus.COMPLETED && isAudio(d.path) && (
                    <button className="xs" onClick={() => play(d.path)}>
                      ▶
                    </button>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}

      <h2>Uploads</h2>
      {uploads.length === 0 && <p className="muted">No uploads served this session.</p>}
      {uploads.length > 0 && (
        <table>
          <thead>
            <tr>
              <th>File</th>
              <th>User</th>
              <th>State</th>
              <th>Progress</th>
            </tr>
          </thead>
          <tbody>
            {uploads.map((u, i) => (
              <tr key={`${u.username}-${u.filename}-${i}`}>
                <td className="file">{u.filename.replace(/^.*[\\/]/, "")}</td>
                <td>{u.username}</td>
                <td>
                  {u.status === UploadStatus.ACTIVE
                    ? "sending"
                    : u.status === UploadStatus.COMPLETED
                      ? "done"
                      : `failed${u.error ? `: ${u.error}` : ""}`}
                </td>
                <td>
                  {u.size > 0n && (
                    <div className="bar">
                      <div className="bar-fill" style={{ width: `${percent(u.bytes, u.size)}%` }} />
                    </div>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}
