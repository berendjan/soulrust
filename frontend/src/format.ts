// Small display helpers + enum value constants (protobuf-es encodes proto enums
// as numbers and uint64 as bigint).

export function humanSize(bytes: bigint | number): string {
  const n = typeof bytes === "bigint" ? Number(bytes) : bytes;
  if (!Number.isFinite(n) || n <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = n;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return unit === 0 ? `${n} B` : `${value.toFixed(1)} ${units[unit]}`;
}

export function percent(bytes: bigint, size: bigint): number {
  const b = Number(bytes);
  const s = Number(size);
  if (s <= 0) return 0;
  return Math.min(100, Math.max(0, (b / s) * 100));
}

export function lengthStr(secs: number): string {
  if (!secs) return "";
  const m = Math.floor(secs / 60);
  const s = secs % 60;
  const h = Math.floor(m / 60);
  const mm = m % 60;
  return h > 0
    ? `${h}:${String(mm).padStart(2, "0")}:${String(s).padStart(2, "0")}`
    : `${m}:${String(s).padStart(2, "0")}`;
}

// Quality cell mirroring the old UI: lossless "44.1 kHz / 16 bit", else "320 kbps".
export function quality(f: {
  bitrate: number;
  vbr: boolean;
  sampleRate: number;
  bitDepth: number;
}): string {
  if (f.sampleRate && f.bitDepth) {
    const khz = f.sampleRate / 1000;
    const k = Number.isInteger(khz) ? `${khz}` : khz.toFixed(1);
    return `${k} kHz / ${f.bitDepth} bit`;
  }
  if (f.bitrate) return f.vbr ? `${f.bitrate} kbps (vbr)` : `${f.bitrate} kbps`;
  return "";
}

// soulrust.api.v1 enum numeric values.
export const ConnectionState = {
  UNSPECIFIED: 0,
  DISCONNECTED: 1,
  CONNECTING: 2,
  LOGGED_IN: 3,
  LOGIN_FAILED: 4,
} as const;

export const DownloadStatus = {
  UNSPECIFIED: 0,
  QUEUED: 1,
  POSITION: 2,
  STARTING: 3,
  COMPLETED: 4,
  FAILED: 5,
  INCOMPLETE: 6,
  PAUSED: 7,
} as const;

export function downloadStatusLabel(status: number, place: number): string {
  switch (status) {
    case DownloadStatus.QUEUED:
      return "queued";
    case DownloadStatus.POSITION:
      return `queue #${place}`;
    case DownloadStatus.STARTING:
      return "starting";
    case DownloadStatus.COMPLETED:
      return "done";
    case DownloadStatus.FAILED:
      return "failed";
    case DownloadStatus.INCOMPLETE:
      return "incomplete";
    case DownloadStatus.PAUSED:
      return "paused";
    default:
      return "";
  }
}

export const UploadStatus = {
  UNSPECIFIED: 0,
  ACTIVE: 1,
  COMPLETED: 2,
  FAILED: 3,
} as const;

export const UpdaterStatusKind = {
  UNSPECIFIED: 0,
  CHECKING: 1,
  UP_TO_DATE: 2,
  AVAILABLE: 3,
  DOWNLOADING: 4,
  READY_TO_APPLY: 5,
  RESTART_REQUIRED: 6,
  FAILED: 7,
  SKIPPED: 8,
} as const;

// A finished download can be played in the browser via the /media range endpoint.
export function isAudio(path: string): boolean {
  return /\.(mp3|flac|wav|m4a|m4b|aac|mp4|ogg|opus|aiff|aif)$/i.test(path);
}

export function basename(path: string): string {
  return path.replace(/^.*[\\/]/, "");
}

export function dirname(path: string): string {
  const i = Math.max(path.lastIndexOf("/"), path.lastIndexOf("\\"));
  return i >= 0 ? path.slice(0, i) : "";
}

// Bitrate to sort/filter on: the advertised value, or — for lossless that only
// gave sample rate + bit depth — Nicotine+'s estimate (sr × depth × 2 / 1000).
export function effectiveBitrate(f: { bitrate: number; sampleRate: number; bitDepth: number }): number {
  if (f.bitrate) return f.bitrate;
  if (f.sampleRate && f.bitDepth) return Math.floor((f.sampleRate * f.bitDepth * 2) / 1000);
  return 0;
}
