// Root component. For the pipeline slice this renders the single StatusView
// (StatusService.GetStatus); Search / Transfers / Browse / Shares / Config
// views join here as those services land in soulrust.api.v1.
import { StatusView } from "./views/StatusView";

export function App() {
  return (
    <main>
      <h1>soulrust</h1>
      <StatusView />
    </main>
  );
}
