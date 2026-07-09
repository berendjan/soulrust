// Uploads monitor: files served to peers this session (read-only).
import { transfersClient } from "../client";
import { useWatch } from "../useWatch";
import { basename, humanSize, percent, UploadStatus } from "../format";
import type { Transfers, Upload } from "../gen/soulrust/api/v1/api_pb";

export function UploadsView() {
  const transfers = useWatch<Transfers>((signal) => transfersClient.watchTransfers({}, { signal }));
  const all = transfers?.uploads ?? [];
  const active = all.filter((u) => u.status === UploadStatus.ACTIVE);
  const previous = all.filter((u) => u.status !== UploadStatus.ACTIVE);

  const statusPill = (u: Upload) => {
    if (u.status === UploadStatus.ACTIVE) return <span className="pill ok">uploading…</span>;
    if (u.status === UploadStatus.COMPLETED) return <span className="pill ok">done</span>;
    return <span className="pill warn">failed{u.error ? ` — ${u.error}` : ""}</span>;
  };

  const row = (u: Upload, i: number) => (
    <tr key={`${u.username}-${u.filename}-${i}`}>
      <td className="col-file" title={u.filename}>
        {basename(u.filename)}
      </td>
      <td>{u.username}</td>
      <td>
        {statusPill(u)}
        {u.size > 0n && u.status === UploadStatus.ACTIVE && (
          <>
            {" "}
            <span className="bar">
              <span className="bar-fill" style={{ width: `${percent(u.bytes, u.size)}%` }} />
            </span>{" "}
            <span className="muted">{humanSize(u.bytes)}</span>
          </>
        )}
      </td>
    </tr>
  );

  return (
    <>
      <h1>Uploads</h1>
      <p className="sub">Files other users are downloading from you.</p>

      <div className="card">
        <h3>Active — {active.length}</h3>
        {active.length === 0 ? <p className="muted">Nothing uploading right now.</p> : <Table>{active.map(row)}</Table>}
      </div>

      <div className="card">
        <h3>Previous — {previous.length}</h3>
        {previous.length === 0 ? (
          <p className="muted">No uploads served yet this session.</p>
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
          </tr>
        </thead>
        <tbody>{children}</tbody>
      </table>
    </div>
  );
}
