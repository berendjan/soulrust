// Spotify: credentials + OAuth connect, its own page (as in the old UI).
import { useEffect, useState } from "react";

import { configClient } from "../client";
import { patchConfig } from "../configIO";

export function SpotifyView() {
  const [clientId, setClientId] = useState("");
  const [clientSecret, setClientSecret] = useState("");
  const [connected, setConnected] = useState(false);
  const [banner, setBanner] = useState<string | null>(null);

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
    try {
      const err = await patchConfig((init) => {
        init.spotify.clientId = clientId;
        init.spotify.clientSecret = clientSecret;
      });
      setBanner(err ? `error: ${err}` : "saved");
      setClientSecret("");
    } catch (err) {
      setBanner(String(err));
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
            <button className="btn" type="submit">
              Save
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
