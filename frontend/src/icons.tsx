// Inline SVG icons, ported verbatim from the old htmx UI (ui_theme.rs). They
// use stroke="currentColor" so nav items inherit their text color.

export function LogoIcon() {
  return (
    <svg viewBox="0 0 26 26" fill="none">
      <rect x="1" y="1" width="24" height="24" rx="7" fill="currentColor" />
      <path d="M9.5 7v12" stroke="#fbfafa" strokeWidth="1.8" strokeLinecap="round" />
      <path
        d="M14 10.5c0-1.2 1-2 2.4-2 1 0 1.8.35 2.3.9M19 15.5c0 1.3-1.1 2.1-2.6 2.1-1.1 0-1.9-.4-2.4-1"
        stroke="#fbfafa"
        strokeWidth="1.7"
        strokeLinecap="round"
      />
    </svg>
  );
}

export function SearchIcon() {
  return (
    <svg viewBox="0 0 20 20" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round">
      <circle cx="9" cy="9" r="6" />
      <path d="M13.5 13.5L18 18" />
    </svg>
  );
}

export function DownloadIcon() {
  return (
    <svg
      viewBox="0 0 20 20"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.7"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      <path d="M10 3v9" />
      <path d="M6.5 9l3.5 3.5L13.5 9" />
      <path d="M4 16.5h12" />
    </svg>
  );
}

export function SpotifyIcon() {
  return (
    <svg viewBox="0 0 20 20" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round">
      <circle cx="10" cy="10" r="8" />
      <path d="M5.8 8.2c3-1 6-.7 8.6.9" />
      <path d="M6.4 10.8c2.4-.7 4.8-.4 6.8 1" />
      <path d="M6.9 13.1c1.8-.5 3.5-.3 5 .8" />
    </svg>
  );
}

export function SettingsIcon() {
  return (
    <svg viewBox="0 0 20 20" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round">
      <path d="M3 6h7" />
      <path d="M14 6h3" />
      <circle cx="12" cy="6" r="2" />
      <path d="M3 14h3" />
      <path d="M10 14h7" />
      <circle cx="8" cy="14" r="2" />
    </svg>
  );
}

export function UserIcon() {
  return (
    <svg viewBox="0 0 20 20" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round">
      <circle cx="10" cy="7" r="3.2" />
      <path d="M4.6 16.4c.9-2.9 2.9-4.3 5.4-4.3s4.5 1.4 5.4 4.3" />
    </svg>
  );
}
