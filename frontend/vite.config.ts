import { defineConfig } from "vite";

// In dev, proxy Connect calls to the Rust connectrpc server so the browser
// hits the Vite origin (no CORS needed) and /api is forwarded to :5031.
export default defineConfig({
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
