// Search: start searches (plain / Spotify / track list), watch results stream
// in per card, and queue downloads.
import { useState } from "react";

import { searchClient, transfersClient } from "../client";
import { useWatch } from "../useWatch";
import { humanSize, lengthStr, quality } from "../format";
import { usePlayer } from "../player";
import type { Search, Searches } from "../gen/soulrust/api/v1/api_pb";

export function SearchView() {
  const searches = useWatch<Searches>((signal) => searchClient.watchSearches({}, { signal }));
  const [input, setInput] = useState("");
  const [source, setSource] = useState("search");
  const [organize, setOrganize] = useState(false);
  const [banner, setBanner] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!input.trim()) return;
    setBusy(true);
    setBanner(null);
    try {
      const res = await searchClient.search({ input, source, organize });
      setBanner(res.error ? res.error : `started ${res.started.length} search(es)`);
      setInput("");
    } catch (err) {
      setBanner(String(err));
    } finally {
      setBusy(false);
    }
  };

  const cards = searches?.searches ?? [];

  return (
    <div className="view">
      <form className="search-form" onSubmit={submit}>
        <input
          placeholder="artist / title, a track list, or a Spotify playlist URL"
          value={input}
          onChange={(e) => setInput(e.target.value)}
        />
        <select value={source} onChange={(e) => setSource(e.target.value)}>
          <option value="search">Search</option>
          <option value="spotify">Spotify</option>
          <option value="tracklist">Track list</option>
        </select>
        <label className="checkbox">
          <input type="checkbox" checked={organize} onChange={(e) => setOrganize(e.target.checked)} />
          Organize
        </label>
        <button type="submit" disabled={busy}>
          {busy ? "…" : "Search"}
        </button>
      </form>
      {banner && <div className="banner">{banner}</div>}

      {cards.length === 0 && <p className="muted">No searches yet.</p>}
      {cards.map((card) => (
        <SearchCard key={card.token} card={card} />
      ))}
    </div>
  );
}

function SearchCard({ card }: { card: Search }) {
  const play = usePlayer();
  const rows = card.results.flatMap((r) => r.files.map((f) => ({ r, f })));

  const download = (username: string, filename: string, size: bigint) =>
    transfersClient
      .startDownload({ username, filename, size, subdir: card.folder, prefix: card.prefix })
      .catch(() => {});

  return (
    <div className="card">
      <div className="card-head">
        <b>{card.query}</b>
        <span className="muted">{rows.length} file(s)</span>
        <button className="xs" onClick={() => searchClient.removeSearch({ token: card.token }).catch(() => {})}>
          ✕
        </button>
      </div>
      {rows.length > 0 && (
        <table>
          <thead>
            <tr>
              <th>User</th>
              <th>File</th>
              <th>Size</th>
              <th>Quality</th>
              <th>Length</th>
              <th>Slot</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {rows.slice(0, 500).map(({ r, f }, i) => (
              <tr key={`${r.username}-${i}`}>
                <td>{r.username}</td>
                <td className="file">{f.name.replace(/^.*[\\/]/, "")}</td>
                <td>{humanSize(f.size)}</td>
                <td>{quality(f)}</td>
                <td>{lengthStr(f.length)}</td>
                <td>{r.freeSlots ? "●" : `#${r.inQueue}`}</td>
                <td className="actions">
                  <button className="xs" onClick={() => download(r.username, f.name, f.size)}>
                    Get
                  </button>
                  <button className="xs ghost" onClick={() => play(f.name)} title="preview (if downloaded)">
                    ▶
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}
