// Browse a peer's shared files — embedded on the Search page (as in the old UI).
// The listing arrives asynchronously and streams in via WatchBrowse.
import { useState } from "react";

import { browseClient, transfersClient } from "../client";
import { useWatch } from "../useWatch";
import { basename, humanSize } from "../format";
import type { BrowseListings } from "../gen/soulrust/api/v1/api_pb";

export function BrowsePanel() {
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
    <div className="card">
      <h3>Browse a user</h3>
      <form className="search-form" onSubmit={submit}>
        <input placeholder="peer username" value={username} onChange={(e) => setUsername(e.target.value)} />
        <button className="btn" type="submit">
          Browse
        </button>
      </form>
      {banner && <div className="banner" style={{ marginTop: "0.8rem" }}>{banner}</div>}
      {users.map((u) => (
        <details className="dir" key={u.username} open>
          <summary>
            <b>{u.username}</b>{" "}
            {u.error ? (
              <span className="pill warn">{u.error}</span>
            ) : (
              <span className="muted">
                {Number(u.totalFiles)} file(s){u.truncated ? " (partial)" : ""}
              </span>
            )}
          </summary>
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
                        {basename(f.name)}
                      </button>
                      <span className="muted"> ({humanSize(f.size)})</span>
                    </li>
                  ))}
                </ul>
              </details>
            ))}
        </details>
      ))}
    </div>
  );
}
