import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

const API = "http://127.0.0.1:5031";

// In dev, proxy everything the Rust server owns to it, so the browser only ever
// talks to the Vite origin (no CORS needed): `/api` carries the Connect calls,
// and `/media` + `/spotify` are the plain HTTP routes the audio player and the
// OAuth flow depend on. In production the bundle is embedded in and served by
// the Rust binary itself, same-origin — every one of these paths is then a
// normal same-port request (see crates/soulrust/src/components/api_server.rs).
export default defineConfig({
  plugins: [react()],
  server: {
    proxy: {
      "/api": {
        target: API,
        changeOrigin: true,
        rewrite: (path) => path.replace(/^\/api/, ""),
      },
      "/media": { target: API, changeOrigin: true },
      "/spotify": { target: API, changeOrigin: true },
    },
  },
});
