// Spotify: credentials + OAuth connect, its own page (as in the old UI).
import { useEffect, useState } from "react";

import { configClient } from "../client";
import { patchConfig } from "../configIO";

export function SpotifyView() {
  const [clientId, setClientId] = useState("");
  const [clientSecret, setClientSecret] = useState("");
  const [connected, setConnected] = useState(false);
  const [banner, setBanner] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    configClient
      .getConfig({})
      .then((c) => {
        setClientId(c.spotify?.clientId ?? "");
        setConnected(c.spotify?.connected ?? false);
      })
      .catch((e) => setBanner(String(e)));
  }, []);

  const save = async (e: React.FormEvent) => {
    e.preventDefault();
    setBanner(null);
    setBusy(true);
    try {
      const err = await patchConfig((init) => {
        init.spotify.clientId = clientId;
        init.spotify.clientSecret = clientSecret;
      });
      setClientSecret("");
      if (err) {
        setBanner(`error: ${err}`);
        return;
      }
      // Saving a typo'd key should say so now, not fail opaquely later when the
      // OAuth flow starts: ask Spotify for an app token with what we just stored.
      const check = await configClient.verifySpotify({});
      if (check.unset) setBanner("saved");
      else if (check.error) setBanner(`saved, but Spotify rejected the credentials: ${check.error}`);
      else setBanner("saved and verified — now click “Connect Spotify”");
    } catch (err) {
      setBanner(String(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <h1>Spotify</h1>
      <p className="sub">
        Connect Spotify to expand playlist and album links into searches on the Search page.
      </p>

      <div className="card">
        <p>
          {connected ? (
            <span className="pill ok">● connected</span>
          ) : (
            <span className="pill warn">● not connected</span>
          )}
        </p>
        <ol className="steps">
          <li>Create an app at the Spotify developer dashboard.</li>
          <li>
            Set its redirect URI to <code>http://127.0.0.1:5031/spotify/callback</code>.
          </li>
          <li>Paste the Client ID and Secret below and save.</li>
          <li>Click “Connect Spotify” to authorize.</li>
        </ol>
      </div>

      <div className="card">
        {banner && <div className="banner">{banner}</div>}
        <form onSubmit={save}>
          <label className="field">
            <span>Client ID</span>
            <input value={clientId} onChange={(e) => setClientId(e.target.value)} />
          </label>
          <label className="field">
            <span>Client secret</span>
            <input
              type="password"
              value={clientSecret}
              placeholder="leave blank to keep current"
              onChange={(e) => setClientSecret(e.target.value)}
            />
          </label>
          <div className="row">
            <button className="btn" type="submit" disabled={busy}>
              {busy ? "…" : "Save"}
            </button>
            <a className="btn spotify" href="/spotify/login">
              Connect Spotify
            </a>
          </div>
        </form>
      </div>
    </>
  );
}
