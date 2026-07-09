// Downloads: Active + Previous cards, matching the old two-section layout.
import { systemClient, transfersClient } from "../client";
import { useWatch } from "../useWatch";
import { basename, DownloadStatus, downloadStatusLabel, isAudio, percent } from "../format";
import { usePlayer } from "../player";
import type { Download, Transfers } from "../gen/soulrust/api/v1/api_pb";

const ACTIVE = new Set<number>([DownloadStatus.QUEUED, DownloadStatus.POSITION, DownloadStatus.STARTING]);

export function DownloadsView() {
  const transfers = useWatch<Transfers>((signal) => transfersClient.watchTransfers({}, { signal }));
  const play = usePlayer();
  const all = transfers?.downloads ?? [];
  const active = all.filter((d) => ACTIVE.has(d.status));
  const previous = all.filter((d) => !ACTIVE.has(d.status));

  const resume = (d: Download) =>
    transfersClient
      .startDownload({
        username: d.username,
        filename: d.filename,
        size: d.size,
        subdir: d.subdir,
        prefix: d.prefix,
      })
      .catch(() => {});

  const statusPill = (d: Download) => {
    const label = downloadStatusLabel(d.status, d.place);
    if (d.status === DownloadStatus.COMPLETED) return <span className="pill ok">{label}</span>;
    if (d.status === DownloadStatus.FAILED || d.status === DownloadStatus.PAUSED || d.status === DownloadStatus.INCOMPLETE)
      return <span className="pill warn">{label}</span>;
    if (d.status === DownloadStatus.STARTING) return <span className="pill ok">downloading…</span>;
    return <span className="pill">{label}</span>;
  };

  const row = (d: Download, i: number) => (
    <tr key={`${d.username}-${d.filename}-${i}`}>
      <td className="col-file" title={d.filename}>
        {basename(d.filename) || "(unknown)"}
      </td>
      <td>{d.username || "—"}</td>
      <td>
        {statusPill(d)}
        {d.status === DownloadStatus.FAILED && d.error ? <span className="muted"> {d.error}</span> : null}
        {d.size > 0n && ACTIVE.has(d.status) && (
          <>
            {" "}
            <span className="bar">
              <span className="bar-fill" style={{ width: `${percent(d.bytes, d.size)}%` }} />
            </span>{" "}
            <span className="muted">{Math.round(percent(d.bytes, d.size))}%</span>
          </>
        )}
      </td>
      <td className="actions">
        {ACTIVE.has(d.status) && (
          <>
            <button
              className="btn xs secondary"
              onClick={() => transfersClient.pauseDownload({ username: d.username, filename: d.filename }).catch(() => {})}
            >
              Pause
            </button>
            <button
              className="btn xs secondary"
              onClick={() => transfersClient.cancelDownload({ username: d.username, filename: d.filename }).catch(() => {})}
            >
              ✕
            </button>
          </>
        )}
        {/* A paused transfer keeps the peer, size and destination it started
            with, so it can be picked up again where it left off. */}
        {d.status === DownloadStatus.PAUSED && d.username && (
          <>
            <button className="btn xs" onClick={() => resume(d)}>
              Resume
            </button>
            <button
              className="btn xs secondary"
              title="cancel"
              onClick={() => transfersClient.cancelDownload({ username: d.username, filename: d.filename }).catch(() => {})}
            >
              ✕
            </button>
          </>
        )}
        {d.status === DownloadStatus.COMPLETED && isAudio(d.path) && (
          <button className="btn xs" onClick={() => play(d.path)}>
            ▶
          </button>
        )}
        {d.status === DownloadStatus.COMPLETED && d.path && (
          <button
            className="btn xs secondary"
            title="open containing folder"
            onClick={() => systemClient.openPath({ path: d.path }).catch(() => {})}
          >
            ↗
          </button>
        )}
      </td>
    </tr>
  );

  return (
    <>
      <h1>Downloads</h1>
      <p className="sub">Files you're grabbing from peers.</p>

      <div className="card">
        <h3>Active — {active.length}</h3>
        {active.length === 0 ? (
          <p className="muted">Nothing downloading right now.</p>
        ) : (
          <Table>{active.map(row)}</Table>
        )}
      </div>

      <div className="card">
        <h3>Previous — {previous.length}</h3>
        {previous.length === 0 ? (
          <p className="muted">No finished downloads yet.</p>
        ) : (
          <Table>{previous.map(row)}</Table>
        )}
      </div>
    </>
  );
}

function Table({ children }: { children: React.ReactNode }) {
  return (
    <div className="results-scroll">
      <table className="results-table">
        <thead>
          <tr>
            <th>File</th>
            <th>User</th>
            <th>Status</th>
            <th></th>
          </tr>
        </thead>
        <tbody>{children}</tbody>
      </table>
    </div>
  );
}
