// Search page: status panel, a search/bulk form, and per-search result cards.
// Bulk (playlist/album) searches are grouped under one outer card titled with
// the playlist name, each inner card numbered by track. The results table
// supports client-side sort, a min-bitrate filter, and column show/hide. A card
// title can be edited in place (pencil) to re-run the search, replacing the card.
import { Fragment, useEffect, useMemo, useRef, useState } from "react";

import { searchClient, transfersClient } from "../client";
import { useWatch } from "../useWatch";
import { basename, dirname, effectiveBitrate, humanSize, isAudio, lengthStr, quality } from "../format";
import { usePlayer } from "../player";
import { EditIcon, FolderIcon } from "../icons";
import { StatusPanel } from "./StatusPanel";
import { BrowsePanel } from "./BrowsePanel";
import type { Search, Searches, SearchResult, ResultFile } from "../gen/soulrust/api/v1/api_pb";

type Row = { r: SearchResult; f: ResultFile };

interface TableControls {
  sortKey: string | null;
  sortDesc: boolean;
  toggleSort: (key: string) => void;
  minBitrate: number;
  hidden: Set<string>;
  searchAgain: (token: number, query: string) => void;
}

// Toggleable columns (File is always shown), matching the old column bar.
const COLUMNS: { key: string; label: string; num: boolean; toggle: boolean }[] = [
  { key: "user", label: "User", num: false, toggle: true },
  { key: "folder", label: "Folder", num: false, toggle: true },
  { key: "file", label: "File", num: false, toggle: false },
  { key: "size", label: "Size", num: true, toggle: true },
  { key: "bitrate", label: "Bitrate", num: true, toggle: true },
  { key: "length", label: "Length", num: true, toggle: true },
  { key: "slot", label: "Slot", num: false, toggle: true },
  { key: "speed", label: "Speed B/s", num: true, toggle: true },
  { key: "queue", label: "Queue", num: true, toggle: true },
];

export function SearchView() {
  const searches = useWatch<Searches>((signal) => searchClient.watchSearches({}, { signal }));
  const [input, setInput] = useState("");
  const [organize, setOrganize] = useState(false);
  const [banner, setBanner] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // Table controls (shared across cards, like the old page-level column bar).
  const [sortKey, setSortKey] = useState<string | null>(null);
  const [sortDesc, setSortDesc] = useState(false);
  const [minBitrate, setMinBitrate] = useState(0);
  const [hidden, setHidden] = useState<Set<string>>(new Set());

  // Display order of tokens — lets a re-searched card land back in place.
  const [order, setOrder] = useState<number[]>([]);
  const placement = useRef<Map<number, number>>(new Map());

  const cards = useMemo(() => searches?.searches ?? [], [searches]);

  useEffect(() => {
    const live = new Set(cards.map((c) => c.token));
    setOrder((prev) => {
      const next = prev.filter((t) => live.has(t));
      for (const c of cards) {
        if (next.includes(c.token)) continue;
        const at = placement.current.get(c.token);
        if (at != null && at <= next.length) {
          next.splice(at, 0, c.token);
          placement.current.delete(c.token);
        } else {
          next.push(c.token);
        }
      }
      const same = next.length === prev.length && next.every((t, i) => t === prev[i]);
      return same ? prev : next;
    });
  }, [cards]);

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!input.trim()) return;
    setBusy(true);
    setBanner(null);
    try {
      const res = await searchClient.search({ input, organize });
      setBanner(res.error ? res.error : `started ${res.started.length} search(es)`);
      setInput("");
    } catch (err) {
      setBanner(String(err));
    } finally {
      setBusy(false);
    }
  };

  const searchAgain = (token: number, query: string) => {
    if (!query.trim()) return;
    const idx = order.indexOf(token);
    searchClient
      .search({ input: query, replaceToken: token })
      .then((res) => res.started.forEach((s) => placement.current.set(s.token, idx < 0 ? order.length : idx)))
      .catch(() => {});
  };

  const toggleSort = (key: string) => {
    if (sortKey === key) setSortDesc((d) => !d);
    else {
      setSortKey(key);
      setSortDesc(false);
    }
  };
  const toggleCol = (key: string) =>
    setHidden((prev) => {
      const n = new Set(prev);
      n.has(key) ? n.delete(key) : n.add(key);
      return n;
    });

  const controls: TableControls = { sortKey, sortDesc, toggleSort, minBitrate, hidden, searchAgain };

  // Build ordered items, grouping bulk (playlist/album) searches by their shared
  // group name. Standalone searches (empty group) render on their own.
  const byToken = new Map(cards.map((c) => [c.token, c]));
  type Item = { group: string; searches: Search[] } | { single: Search };
  const items: Item[] = [];
  const groupAt = new Map<string, number>();
  for (const token of order) {
    const c = byToken.get(token);
    if (!c) continue;
    if (c.group) {
      if (groupAt.has(c.group)) {
        (items[groupAt.get(c.group)!] as { searches: Search[] }).searches.push(c);
      } else {
        groupAt.set(c.group, items.length);
        items.push({ group: c.group, searches: [c] });
      }
    } else {
      items.push({ single: c });
    }
  }

  return (
    <>
      <h1>Search</h1>
      <p className="sub">Search the network, or paste a Spotify track / album / playlist URL to grab in bulk.</p>

      <StatusPanel />

      <div className="card">
        <form className="search-form" onSubmit={submit}>
          <input
            autoFocus
            placeholder="artist / title, or a Spotify track / album / playlist URL"
            value={input}
            onChange={(e) => setInput(e.target.value)}
          />
          <label className="checkbox" style={{ marginTop: 0 }} title="organize a playlist/album into a numbered subfolder">
            <input type="checkbox" checked={organize} onChange={(e) => setOrganize(e.target.checked)} />
            Organize
          </label>
          <button className="btn" type="submit" disabled={busy}>
            {busy ? "…" : "Search"}
          </button>
        </form>
        {banner && <div className="banner" style={{ marginTop: "0.8rem" }}>{banner}</div>}
      </div>

      {items.length > 0 && <ColBar minBitrate={minBitrate} setMinBitrate={setMinBitrate} hidden={hidden} toggleCol={toggleCol} />}

      {items.length === 0 && <p className="muted">No searches yet.</p>}
      {items.map((it, i) =>
        "single" in it ? (
          <SearchCard key={it.single.token} card={it.single} controls={controls} />
        ) : (
          <GroupCard key={`g-${it.group}-${i}`} folder={it.group} searches={it.searches} controls={controls} />
        ),
      )}

      <BrowsePanel />
    </>
  );
}

function ColBar(props: {
  minBitrate: number;
  setMinBitrate: (n: number) => void;
  hidden: Set<string>;
  toggleCol: (key: string) => void;
}) {
  return (
    <div className="col-bar">
      <label>
        Min bitrate{" "}
        <input
          type="number"
          value={props.minBitrate || ""}
          style={{ width: "5.5rem" }}
          onChange={(e) => props.setMinBitrate(Number(e.target.value) || 0)}
        />
      </label>
      <span className="col-bar-label">Columns</span>
      {COLUMNS.filter((c) => c.toggle).map((c) => (
        <label key={c.key} className="checkbox" style={{ marginTop: 0 }}>
          <input type="checkbox" checked={!props.hidden.has(c.key)} onChange={() => props.toggleCol(c.key)} />
          {c.label}
        </label>
      ))}
    </div>
  );
}

function GroupCard(props: { folder: string; searches: Search[]; controls: TableControls }) {
  const total = props.searches.reduce((n, s) => n + s.results.reduce((m, r) => m + r.files.length, 0), 0);
  return (
    <div className="card group-card">
      <div className="card-head">
        <span className="ico" style={{ width: 20 }}>
          <FolderIcon />
        </span>
        <h2 style={{ margin: 0 }}>{props.folder}</h2>
        <span className="muted">
          — {props.searches.length} track(s), {total} file(s)
        </span>
      </div>
      {props.searches.map((s) => (
        <SearchCard key={s.token} card={s} controls={props.controls} trackNo={s.track || undefined} inGroup />
      ))}
    </div>
  );
}

function SearchCard(props: { card: Search; controls: TableControls; trackNo?: number; inGroup?: boolean }) {
  const { card, controls, trackNo, inGroup } = props;
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(card.query);
  const fileCount = card.results.reduce((n, r) => n + r.files.length, 0);

  const submitRename = (e: React.FormEvent) => {
    e.preventDefault();
    controls.searchAgain(card.token, draft);
    setEditing(false);
  };

  return (
    <div className={inGroup ? "subcard" : "card"}>
      <div className="card-head">
        {trackNo ? <span className="pill" title={`track ${trackNo}`}>{trackNo}</span> : null}
        {editing ? (
          <form className="search-form" style={{ flex: 1 }} onSubmit={submitRename}>
            <input value={draft} autoFocus onChange={(e) => setDraft(e.target.value)} />
            <button className="btn xs" type="submit">
              Search again
            </button>
            <button className="btn xs secondary" type="button" onClick={() => setEditing(false)}>
              Cancel
            </button>
          </form>
        ) : (
          <>
            <b>{card.query}</b>
            <button
              className="btn xs secondary icon-btn"
              title="edit & search again"
              onClick={() => {
                setDraft(card.query);
                setEditing(true);
              }}
            >
              <span className="ico" style={{ width: 14, height: 14 }}>
                <EditIcon />
              </span>
            </button>
            <span className="muted">
              {card.results.length} peer(s), {fileCount} file(s)
            </span>
            <button
              className="btn xs secondary spacer"
              title="close"
              onClick={() => searchClient.removeSearch({ token: card.token }).catch(() => {})}
            >
              ✕
            </button>
          </>
        )}
      </div>
      {fileCount > 0 && <ResultsTable card={card} controls={controls} />}
    </div>
  );
}

function ResultsTable({ card, controls }: { card: Search; controls: TableControls }) {
  const play = usePlayer();
  const { sortKey, sortDesc, toggleSort, minBitrate, hidden } = controls;

  let rows: Row[] = card.results.flatMap((r) => r.files.map((f) => ({ r, f })));
  if (minBitrate > 0) rows = rows.filter(({ f }) => effectiveBitrate(f) >= minBitrate);
  if (sortKey) {
    rows = [...rows].sort((a, b) => compareRows(sortKey, a, b));
    if (sortDesc) rows.reverse();
  }

  const download = (username: string, filename: string, size: bigint) =>
    transfersClient
      .startDownload({ username, filename, size, subdir: card.folder, prefix: card.prefix })
      .catch(() => {});

  const cell = (key: string, r: SearchResult, f: ResultFile) => {
    switch (key) {
      case "user":
        return <td>{r.username}</td>;
      case "folder":
        return (
          <td className="col-folder" title={f.name}>
            {dirname(f.name)}
          </td>
        );
      case "file":
        return (
          <td className="col-file" title={f.name}>
            {basename(f.name)}
          </td>
        );
      case "size":
        return <td className="num">{humanSize(f.size)}</td>;
      case "bitrate":
        return <td className="num">{quality(f)}</td>;
      case "length":
        return <td className="num">{lengthStr(f.length)}</td>;
      case "slot":
        return <td>{r.freeSlots ? <span className="pill ok">free</span> : <span className="pill warn">queued</span>}</td>;
      case "speed":
        return <td className="num">{r.uploadSpeed ? humanSize(r.uploadSpeed) : ""}</td>;
      case "queue":
        return <td className="num">{r.inQueue || ""}</td>;
      default:
        return null;
    }
  };

  return (
    <div className="results-scroll">
      <table className="results-table">
        <thead>
          <tr>
            {COLUMNS.filter((c) => !hidden.has(c.key)).map((c) => (
              <th
                key={c.key}
                className={`sort ${c.num ? "num" : ""} ${sortKey === c.key ? "sorted" : ""}`}
                onClick={() => toggleSort(c.key)}
              >
                {c.label}
                {sortKey === c.key ? (sortDesc ? " ▼" : " ▲") : ""}
              </th>
            ))}
            <th></th>
          </tr>
        </thead>
        <tbody>
          {rows.slice(0, 1000).map(({ r, f }, i) => (
            <tr key={`${r.username}-${i}`}>
              {COLUMNS.filter((c) => !hidden.has(c.key)).map((c) => (
                <Fragment key={c.key}>{cell(c.key, r, f)}</Fragment>
              ))}
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
  );
}

function compareRows(key: string, a: Row, b: Row): number {
  switch (key) {
    case "user":
      return a.r.username.localeCompare(b.r.username);
    case "folder":
      return dirname(a.f.name).localeCompare(dirname(b.f.name));
    case "file":
      return basename(a.f.name).localeCompare(basename(b.f.name));
    case "size":
      return Number(a.f.size - b.f.size);
    case "bitrate":
      return effectiveBitrate(a.f) - effectiveBitrate(b.f);
    case "length":
      return a.f.length - b.f.length;
    case "slot":
      return Number(a.r.freeSlots) - Number(b.r.freeSlots);
    case "speed":
      return a.r.uploadSpeed - b.r.uploadSpeed;
    case "queue":
      return a.r.inQueue - b.r.inQueue;
    default:
      return 0;
  }
}
