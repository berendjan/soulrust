// A single shared audio player docked in the sidebar (as in the old UI). The
// context exposes play(); <SidebarPlayer/> renders the now-playing line + the
// native <audio> element, hidden until the first play. Streams from /media.
import { createContext, useContext, useState, type ReactNode } from "react";

interface PlayerState {
  play: (path: string) => void;
  src: string | null;
  label: string;
}

const PlayerContext = createContext<PlayerState>({ play: () => {}, src: null, label: "" });

export function usePlayer(): (path: string) => void {
  return useContext(PlayerContext).play;
}

export function PlayerProvider({ children }: { children: ReactNode }) {
  const [src, setSrc] = useState<string | null>(null);
  const [label, setLabel] = useState("");
  const play = (path: string) => {
    setLabel(path.replace(/^.*[\\/]/, ""));
    setSrc(`/media?path=${encodeURIComponent(path)}`);
  };
  return <PlayerContext.Provider value={{ play, src, label }}>{children}</PlayerContext.Provider>;
}

export function SidebarPlayer() {
  const { src, label } = useContext(PlayerContext);
  if (!src) return null;
  return (
    <div className="player">
      <div className="np" title={label}>
        {label}
      </div>
      <audio id="player" src={src} controls autoPlay preload="none" />
    </div>
  );
}
