import path from "node:path";
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig(({ mode }) => ({
  plugins: [react()],
  base: mode === "production" ? "/static/dashboard/" : "/",
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },
  server: {
    port: 5174,
    proxy: {
      "/analytics": "http://127.0.0.1:7878",
      "/admin": "http://127.0.0.1:7878",
      "/sync": "http://127.0.0.1:7878",
      "/health": "http://127.0.0.1:7878",
      "/hooks": "http://127.0.0.1:7878",
      "/v1": "http://127.0.0.1:7878",
    },
  },
  build: {
    outDir: "../../crates/budi-daemon/static/dashboard-dist",
    emptyOutDir: true,
  },
}));
