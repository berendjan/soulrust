// A single shared audio player, fed by a context any view can call. The browser
// <audio> element streams from the Rust /media?path=… range endpoint.
import { createContext, useContext, useState, type ReactNode } from "react";

const PlayerContext = createContext<(path: string) => void>(() => {});

export function usePlayer(): (path: string) => void {
  return useContext(PlayerContext);
}

export function PlayerProvider({ children }: { children: ReactNode }) {
  const [src, setSrc] = useState<string | null>(null);
  const [label, setLabel] = useState("");

  const play = (path: string) => {
    setLabel(path.replace(/^.*[\\/]/, ""));
    setSrc(`/media?path=${encodeURIComponent(path)}`);
  };

  return (
    <PlayerContext.Provider value={play}>
      {children}
      {src && (
        <div className="player">
          <span className="player-label">{label}</span>
          <audio src={src} controls autoPlay />
        </div>
      )}
    </PlayerContext.Provider>
  );
}
