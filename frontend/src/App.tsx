// The app shell: a collapsible left icon-sidebar (brand + nav + sidebar player
// + account chip) and a fluid main content area — matching the pre-migration
// htmx layout. Tabs swap the main view client-side.
import { useState } from "react";

import { statusClient } from "./client";
import { useWatch } from "./useWatch";
import { ConnectionState } from "./format";
import { PlayerProvider, SidebarPlayer } from "./player";
import { DownloadIcon, LogoIcon, SearchIcon, SettingsIcon, SpotifyIcon, UserIcon } from "./icons";
import { SearchView } from "./views/SearchView";
import { DownloadsView } from "./views/DownloadsView";
import { UploadsView } from "./views/UploadsView";
import { SpotifyView } from "./views/SpotifyView";
import { SettingsView } from "./views/SettingsView";
import type { Status } from "./gen/soulrust/api/v1/api_pb";

const TABS = [
  { id: "search", label: "Search", icon: <SearchIcon />, view: <SearchView /> },
  { id: "downloads", label: "Downloads", icon: <DownloadIcon />, view: <DownloadsView /> },
  { id: "uploads", label: "Uploads", icon: <DownloadIcon />, view: <UploadsView /> },
  { id: "spotify", label: "Spotify", icon: <SpotifyIcon />, view: <SpotifyView /> },
  { id: "config", label: "Settings", icon: <SettingsIcon />, view: <SettingsView /> },
] as const;

type TabId = (typeof TABS)[number]["id"];

export function App() {
  const [tab, setTab] = useState<TabId>("search");
  const status = useWatch<Status>((signal) => statusClient.watchStatus({}, { signal }));

  return (
    <PlayerProvider>
      <input type="checkbox" id="nav-collapse" className="nav-collapse" />
      <div className="layout">
        <aside className="sidebar">
          <label className="brand" htmlFor="nav-collapse" title="Collapse / expand the sidebar">
            <span className="logo">
              <LogoIcon />
            </span>
            <span className="brand-name">soulrust</span>
          </label>
          <nav className="nav">
            {TABS.map((t) => (
              <button
                key={t.id}
                className={t.id === tab ? "active" : ""}
                onClick={() => setTab(t.id)}
                title={t.label}
              >
                <span className="ico">{t.icon}</span>
                <span className="label">{t.label}</span>
              </button>
            ))}
          </nav>
          <SidebarPlayer />
          <div className="nav-footer">
            <button className="user" onClick={() => setTab("config")} title="Account & settings">
              <span className="avatar">
                <UserIcon />
              </span>
              <span className="who">
                <span className="name">{status?.username || "soulrust"}</span>
                <span className="role">{connectionLabel(status)}</span>
              </span>
            </button>
          </div>
        </aside>
        <main className="main">{TABS.find((t) => t.id === tab)?.view}</main>
      </div>
    </PlayerProvider>
  );
}

function connectionLabel(status: Status | null): string {
  switch (status?.connection) {
    case ConnectionState.LOGGED_IN:
      return "connected";
    case ConnectionState.CONNECTING:
      return "connecting…";
    case ConnectionState.LOGIN_FAILED:
      return "sign-in failed";
    default:
      return "offline";
  }
}
