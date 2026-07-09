import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// In dev, proxy Connect calls to the Rust connectrpc server so the browser
// hits the Vite origin (no CORS needed) and /api is forwarded to :5031. In
// production the bundle is embedded in and served by the Rust binary itself,
// same-origin (see crates/soulrust/src/components/api_server.rs).
export default defineConfig({
  plugins: [react()],
  server: {
    proxy: {
      "/api": {
        target: "http://127.0.0.1:5031",
        changeOrigin: true,
        rewrite: (path) => path.replace(/^\/api/, ""),
      },
    },
  },
});
