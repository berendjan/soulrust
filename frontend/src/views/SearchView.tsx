// Search page: the status panel, a search/bulk form, per-search result cards
// with the dense sortable results table, and the embedded browse panel — the
// composition of the old htmx index page.
import { useState } from "react";

import { searchClient, transfersClient } from "../client";
import { useWatch } from "../useWatch";
import { basename, dirname, humanSize, isAudio, lengthStr, quality } from "../format";
import { usePlayer } from "../player";
import { StatusPanel } from "./StatusPanel";
import { BrowsePanel } from "./BrowsePanel";
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
    <>
      <h1>Search</h1>
      <p className="sub">Search the network, or paste a Spotify playlist / track list to grab in bulk.</p>

      <StatusPanel />

      <div className="card">
        <form className="search-form" onSubmit={submit}>
          <input
            autoFocus
            placeholder="artist / title, a track list, or a Spotify playlist URL"
            value={input}
            onChange={(e) => setInput(e.target.value)}
          />
          <select value={source} onChange={(e) => setSource(e.target.value)}>
            <option value="search">Search</option>
            <option value="spotify">Spotify</option>
            <option value="tracklist">Track list</option>
          </select>
          <label className="checkbox" style={{ marginTop: 0 }}>
            <input type="checkbox" checked={organize} onChange={(e) => setOrganize(e.target.checked)} />
            Organize
          </label>
          <button className="btn" type="submit" disabled={busy}>
            {busy ? "…" : "Search"}
          </button>
        </form>
        {banner && <div className="banner" style={{ marginTop: "0.8rem" }}>{banner}</div>}
      </div>

      {cards.map((card) => (
        <SearchCard key={card.token} card={card} />
      ))}

      <BrowsePanel />
    </>
  );
}

const COLUMNS = ["User", "Folder", "File", "Size", "Bitrate", "Length", "Slot", "Speed B/s", "Queue", ""];

function SearchCard({ card }: { card: Search }) {
  const play = usePlayer();
  const rows = card.results.flatMap((r) => r.files.map((f) => ({ r, f })));
  const peers = card.results.length;

  const download = (username: string, filename: string, size: bigint) =>
    transfersClient
      .startDownload({ username, filename, size, subdir: card.folder, prefix: card.prefix })
      .catch(() => {});

  return (
    <div className="card">
      <div className="card-head">
        <h3 style={{ margin: 0 }}>{card.query}</h3>
        <span className="muted">
          — {peers} peer(s), {rows.length} file(s)
        </span>
        <button
          className="btn xs secondary spacer"
          title="close"
          onClick={() => searchClient.removeSearch({ token: card.token }).catch(() => {})}
        >
          ✕
        </button>
      </div>
      {rows.length > 0 && (
        <div className="results-scroll">
          <table className="results-table">
            <thead>
              <tr>
                {COLUMNS.map((c, i) => (
                  <th key={i} className={i >= 3 && i <= 8 && c ? "num" : ""}>
                    {c}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {rows.slice(0, 1000).map(({ r, f }, i) => (
                <tr key={`${r.username}-${i}`}>
                  <td>{r.username}</td>
                  <td className="col-folder" title={f.name}>
                    {dirname(f.name)}
                  </td>
                  <td className="col-file" title={f.name}>
                    {basename(f.name)}
                  </td>
                  <td className="num">{humanSize(f.size)}</td>
                  <td className="num">{quality(f)}</td>
                  <td className="num">{lengthStr(f.length)}</td>
                  <td>
                    {r.freeSlots ? <span className="pill ok">free</span> : <span className="pill warn">queued</span>}
                  </td>
                  <td className="num">{r.uploadSpeed ? humanSize(r.uploadSpeed) : ""}</td>
                  <td className="num">{r.inQueue || ""}</td>
                  <td className="actions">
                    <button className="btn xs" onClick={() => download(r.username, f.name, f.size)}>
                      Get
                    </button>
                    {isAudio(f.name) && (
                      <button className="btn xs secondary" title="preview (if downloaded)" onClick={() => play(f.name)}>
                        ▶
                      </button>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}
