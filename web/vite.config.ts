import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Dev server proxies API calls to a locally running sandbox-api so the
// console is same-origin in both dev and production (no CORS surface).
export default defineConfig({
  // The API serves the build under /console; assets must resolve there
  // (absolute /assets URLs would 404 behind the /console mount).
  base: "/console/",
  plugins: [react()],
  server: {
    proxy: {
      "/health": "http://127.0.0.1:8094",
      "/metrics": "http://127.0.0.1:8094",
      "/sandboxes": "http://127.0.0.1:8094",
    },
  },
  build: {
    outDir: "dist",
    sourcemap: false,
  },
});
