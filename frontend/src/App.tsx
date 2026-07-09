// Root: a tab bar over the four views, a live status bar, and the shared player.
import { useState } from "react";

import { StatusBar } from "./views/StatusBar";
import { SearchView } from "./views/SearchView";
import { TransfersView } from "./views/TransfersView";
import { BrowseView } from "./views/BrowseView";
import { ConfigView } from "./views/ConfigView";
import { PlayerProvider } from "./player";

const TABS = [
  { id: "search", label: "Search", view: <SearchView /> },
  { id: "transfers", label: "Transfers", view: <TransfersView /> },
  { id: "browse", label: "Browse", view: <BrowseView /> },
  { id: "config", label: "Config", view: <ConfigView /> },
] as const;

export function App() {
  const [tab, setTab] = useState<(typeof TABS)[number]["id"]>("search");

  return (
    <PlayerProvider>
      <header>
        <h1>soulrust</h1>
        <StatusBar />
      </header>
      <nav>
        {TABS.map((t) => (
          <button key={t.id} className={t.id === tab ? "active" : ""} onClick={() => setTab(t.id)}>
            {t.label}
          </button>
        ))}
      </nav>
      <main>{TABS.find((t) => t.id === tab)?.view}</main>
    </PlayerProvider>
  );
}
