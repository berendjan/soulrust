// Browse a peer's shared files. The listing arrives asynchronously and streams
// in via WatchBrowse.
import { useState } from "react";

import { browseClient, transfersClient } from "../client";
import { useWatch } from "../useWatch";
import { humanSize } from "../format";
import type { BrowseListings } from "../gen/soulrust/api/v1/api_pb";

export function BrowseView() {
  const listings = useWatch<BrowseListings>((signal) => browseClient.watchBrowse({}, { signal }));
  const [username, setUsername] = useState("");
  const [banner, setBanner] = useState<string | null>(null);

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!username.trim()) return;
    try {
      const res = await browseClient.browse({ username });
      setBanner(res.error ? res.error : `browsing ${username.trim()}… results appear below`);
    } catch (err) {
      setBanner(String(err));
    }
  };

  const users = listings?.users ?? [];

  return (
    <div className="view">
      <form className="search-form" onSubmit={submit}>
        <input placeholder="peer username" value={username} onChange={(e) => setUsername(e.target.value)} />
        <button type="submit">Browse</button>
      </form>
      {banner && <div className="banner">{banner}</div>}
      {users.length === 0 && <p className="muted">No browses yet.</p>}
      {users.map((u) => (
        <div className="card" key={u.username}>
          <div className="card-head">
            <b>{u.username}</b>
            {u.error ? (
              <span className="pill warn">{u.error}</span>
            ) : (
              <span className="muted">
                {Number(u.totalFiles)} file(s){u.truncated ? " (partial)" : ""}
              </span>
            )}
          </div>
          {!u.error &&
            u.directories.map((dir) => (
              <details key={dir.path} className="dir">
                <summary>{dir.path}</summary>
                <ul>
                  {dir.files.map((f) => (
                    <li key={f.name}>
                      <button
                        className="linklike"
                        title="download"
                        onClick={() =>
                          transfersClient
                            .startDownload({ username: u.username, filename: f.name, size: f.size })
                            .catch(() => {})
                        }
                      >
                        {f.name.replace(/^.*[\\/]/, "")}
                      </button>
                      <span className="muted"> ({humanSize(f.size)})</span>
                    </li>
                  ))}
                </ul>
              </details>
            ))}
        </div>
      ))}
    </div>
  );
}
