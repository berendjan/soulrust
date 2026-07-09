// Settings: account, sharing, updates, interface, and process control. Loads
// the current config once, edits locally, and saves via patchConfig (which
// preserves the sections this view doesn't touch, e.g. Spotify).
import { useEffect, useState } from "react";

import { configClient, systemClient, updaterClient } from "../client";
import { patchConfig } from "../configIO";
import { useWatch } from "../useWatch";
import { UpdaterStatusKind } from "../format";
import type { UpdaterStatus } from "../gen/soulrust/api/v1/api_pb";

interface Form {
  host: string;
  port: number;
  username: string;
  password: string;
  listenPort: number;
  updateEnabled: boolean;
  autoApply: boolean;
  repo: string;
  bindAddr: string;
  openBrowser: boolean;
  folders: string;
  downloadDir: string;
  incompleteDir: string;
  uploadSlots: number;
  respondToSearches: boolean;
  fifoQueue: boolean;
  organizeDownloads: boolean;
  maxSearchResults: number;
  minResultFiles: number;
  minPeerUploadSpeed: number;
  maxPeerQueueLength: number;
  maxDownloadSpeed: number;
  maxUploadSpeed: number;
}

export function SettingsView() {
  const [form, setForm] = useState<Form | null>(null);
  const [banner, setBanner] = useState<string | null>(null);
  const [file, setFile] = useState<{ path: string; yaml: string } | null>(null);
  const updater = useWatch<UpdaterStatus>((signal) => updaterClient.watchUpdater({}, { signal }));

  const viewFile = async () => {
    try {
      const f = await configClient.getConfigFile({});
      setFile({ path: f.path, yaml: f.yaml });
    } catch (err) {
      setBanner(String(err));
    }
  };

  useEffect(() => {
    configClient
      .getConfig({})
      .then((c) =>
        setForm({
          host: c.server?.host ?? "",
          port: c.server?.port ?? 0,
          username: c.server?.username ?? "",
          password: "",
          listenPort: c.server?.listenPort ?? 0,
          updateEnabled: c.update?.enabled ?? false,
          autoApply: c.update?.autoApply ?? false,
          repo: c.update?.repo ?? "",
          bindAddr: c.ui?.bindAddr ?? "",
          openBrowser: c.ui?.openBrowser ?? false,
          folders: (c.sharing?.folders ?? []).join("\n"),
          downloadDir: c.sharing?.downloadDir ?? "",
          incompleteDir: c.sharing?.incompleteDir ?? "",
          uploadSlots: c.sharing?.uploadSlots ?? 0,
          respondToSearches: c.sharing?.respondToSearches ?? false,
          fifoQueue: c.sharing?.fifoQueue ?? false,
          organizeDownloads: c.sharing?.organizeDownloads ?? true,
          maxSearchResults: c.sharing?.maxSearchResults ?? 0,
          minResultFiles: c.sharing?.minResultFiles ?? 0,
          minPeerUploadSpeed: c.sharing?.minPeerUploadSpeed ?? 0,
          maxPeerQueueLength: c.sharing?.maxPeerQueueLength ?? 0,
          maxDownloadSpeed: c.sharing?.maxDownloadSpeed ?? 0,
          maxUploadSpeed: c.sharing?.maxUploadSpeed ?? 0,
        }),
      )
      .catch((e) => setBanner(String(e)));
  }, []);

  if (!form) return <div className="card">Loading settings…</div>;
  const set = <K extends keyof Form>(k: K, v: Form[K]) => setForm({ ...form, [k]: v });

  const save = async (e: React.FormEvent) => {
    e.preventDefault();
    setBanner(null);
    try {
      const err = await patchConfig((init) => {
        init.server = {
          host: form.host,
          port: form.port,
          username: form.username,
          password: form.password,
          listenPort: form.listenPort,
        };
        init.update = { enabled: form.updateEnabled, autoApply: form.autoApply, repo: form.repo };
        init.ui = { bindAddr: form.bindAddr, openBrowser: form.openBrowser };
        init.sharing = {
          folders: form.folders.split("\n").map((s) => s.trim()).filter(Boolean),
          downloadDir: form.downloadDir,
          incompleteDir: form.incompleteDir,
          uploadSlots: form.uploadSlots,
          fifoQueue: form.fifoQueue,
          organizeDownloads: form.organizeDownloads,
          respondToSearches: form.respondToSearches,
          maxSearchResults: form.maxSearchResults,
          minResultFiles: form.minResultFiles,
          minPeerUploadSpeed: form.minPeerUploadSpeed,
          maxPeerQueueLength: form.maxPeerQueueLength,
          maxDownloadSpeed: form.maxDownloadSpeed,
          maxUploadSpeed: form.maxUploadSpeed,
        };
      });
      setBanner(err ? `error: ${err}` : "saved");
    } catch (err) {
      setBanner(String(err));
    }
  };

  return (
    <>
      <h1>Settings</h1>
      <p className="sub">Account, sharing, updates and interface.</p>
      {banner && <div className="banner">{banner}</div>}
      <form onSubmit={save}>
        <fieldset>
          <legend>Account</legend>
          <Field label="Username" value={form.username} onChange={(v) => set("username", v)} />
          <Field
            label="Password"
            type="password"
            value={form.password}
            placeholder="leave blank to keep current"
            onChange={(v) => set("password", v)}
          />
          <Field label="Server host" value={form.host} onChange={(v) => set("host", v)} />
          <NumField label="Server port" value={form.port} onChange={(v) => set("port", v)} />
          <NumField label="Listen port" value={form.listenPort} onChange={(v) => set("listenPort", v)} />
        </fieldset>

        <fieldset>
          <legend>Sharing</legend>
          <label className="field">
            <span>Shared folders (one per line)</span>
            <textarea value={form.folders} rows={3} onChange={(e) => set("folders", e.target.value)} />
          </label>
          <Field label="Download dir" value={form.downloadDir} onChange={(v) => set("downloadDir", v)} />
          <Field label="Incomplete dir" value={form.incompleteDir} onChange={(v) => set("incompleteDir", v)} />
          <NumField label="Upload slots" value={form.uploadSlots} onChange={(v) => set("uploadSlots", v)} />
          <NumField label="Max download B/s (0=∞)" value={form.maxDownloadSpeed} onChange={(v) => set("maxDownloadSpeed", v)} />
          <NumField label="Max upload B/s (0=∞)" value={form.maxUploadSpeed} onChange={(v) => set("maxUploadSpeed", v)} />
          <NumField label="Max search results" value={form.maxSearchResults} onChange={(v) => set("maxSearchResults", v)} />
          <NumField label="Min result files" value={form.minResultFiles} onChange={(v) => set("minResultFiles", v)} />
          <NumField label="Min peer upload B/s" value={form.minPeerUploadSpeed} onChange={(v) => set("minPeerUploadSpeed", v)} />
          <NumField label="Max peer queue length" value={form.maxPeerQueueLength} onChange={(v) => set("maxPeerQueueLength", v)} />
          <Check label="Respond to searches" value={form.respondToSearches} onChange={(v) => set("respondToSearches", v)} />
          <Check label="FIFO queue" value={form.fifoQueue} onChange={(v) => set("fifoQueue", v)} />
          <Check
            label="Organize playlist downloads"
            value={form.organizeDownloads}
            onChange={(v) => set("organizeDownloads", v)}
          />
          <p className="muted" style={{ margin: "0.2rem 0 0", fontSize: "0.85rem" }}>
            When downloading a Spotify playlist or album, save its tracks into a subfolder named after the
            collection, each file prefixed with its track number (01, 02, …) so the folder sorts in order.
          </p>
        </fieldset>

        <fieldset>
          <legend>Updates</legend>
          <Check label="Check for updates" value={form.updateEnabled} onChange={(v) => set("updateEnabled", v)} />
          <Check label="Auto-apply" value={form.autoApply} onChange={(v) => set("autoApply", v)} />
          <Field label="Repo" value={form.repo} onChange={(v) => set("repo", v)} />
          <p className="muted">{updaterLabel(updater)}</p>
          <button type="button" className="btn xs secondary" onClick={() => updaterClient.applyUpdate({}).catch(() => {})}>
            Apply update
          </button>
        </fieldset>

        <fieldset>
          <legend>Interface</legend>
          <Field label="Bind address" value={form.bindAddr} onChange={(v) => set("bindAddr", v)} />
          <Check label="Open browser on start" value={form.openBrowser} onChange={(v) => set("openBrowser", v)} />
        </fieldset>

        <fieldset>
          <legend>Config file</legend>
          <p className="muted">The effective configuration on disk, with secrets redacted.</p>
          <button type="button" className="btn xs secondary" onClick={viewFile}>
            {file ? "Refresh" : "View config file"}
          </button>
          {file && (
            <>
              <p className="muted" style={{ marginBottom: "0.3rem" }}>
                <code>{file.path}</code>
              </p>
              <pre className="log" style={{ maxHeight: "20rem" }}>
                {file.yaml}
              </pre>
            </>
          )}
        </fieldset>

        <div className="row">
          <button className="btn" type="submit">
            Save
          </button>
          <button type="button" className="btn secondary" onClick={() => systemClient.restart({}).catch(() => {})}>
            Restart
          </button>
          <button type="button" className="btn secondary" onClick={() => systemClient.quit({}).catch(() => {})}>
            Quit
          </button>
        </div>
      </form>
    </>
  );
}

function updaterLabel(u: UpdaterStatus | null): string {
  if (!u) return "";
  switch (u.kind) {
    case UpdaterStatusKind.CHECKING:
      return "checking for updates…";
    case UpdaterStatusKind.UP_TO_DATE:
      return `up to date (${u.current})`;
    case UpdaterStatusKind.AVAILABLE:
      return `update available: ${u.latest}`;
    case UpdaterStatusKind.DOWNLOADING:
      return `downloading ${u.latest}…`;
    case UpdaterStatusKind.READY_TO_APPLY:
      return `ready to apply ${u.latest}`;
    case UpdaterStatusKind.RESTART_REQUIRED:
      return `restart to finish updating to ${u.latest}`;
    case UpdaterStatusKind.FAILED:
      return `update failed: ${u.error}`;
    case UpdaterStatusKind.SKIPPED:
      return `update skipped: ${u.reason}`;
    default:
      return "";
  }
}

function Field(props: {
  label: string;
  value: string;
  type?: string;
  placeholder?: string;
  onChange: (v: string) => void;
}) {
  return (
    <label className="field">
      <span>{props.label}</span>
      <input
        type={props.type ?? "text"}
        value={props.value}
        placeholder={props.placeholder}
        onChange={(e) => props.onChange(e.target.value)}
      />
    </label>
  );
}

function NumField(props: { label: string; value: number; onChange: (v: number) => void }) {
  return (
    <label className="field">
      <span>{props.label}</span>
      <input type="number" value={props.value} onChange={(e) => props.onChange(Number(e.target.value) || 0)} />
    </label>
  );
}

function Check(props: { label: string; value: boolean; onChange: (v: boolean) => void }) {
  return (
    <label className="checkbox">
      <input type="checkbox" checked={props.value} onChange={(e) => props.onChange(e.target.checked)} />
      {props.label}
    </label>
  );
}
