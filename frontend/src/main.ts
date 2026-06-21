// Entry point: polls the Connect StatusService and renders the session status.
// This is the first Connect-Web component; Search/Transfers/Browse/Shares views
// follow as their services are added to soulrust.api.v1.
import { statusClient } from "./client";

const statusEl = document.getElementById("status")!;

function escapeHtml(s: string): string {
  return s.replace(/[&<>"]/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]!,
  );
}

async function refreshStatus(): Promise<void> {
  try {
    const status = await statusClient.getStatus({});
    statusEl.innerHTML = status.loggedIn
      ? `<p>Logged in as <b>${escapeHtml(status.username)}</b> (${escapeHtml(
          status.ownIp,
        )}) — ${escapeHtml(status.greeting)}</p>
         <p>Sharing ${status.sharedFiles} file(s).</p>`
      : `<p>Not connected.</p>`;
  } catch (err) {
    statusEl.innerHTML = `<p class="error">API error: ${escapeHtml(
      String(err),
    )}</p>`;
  }
}

void refreshStatus();
setInterval(() => void refreshStatus(), 2000);
