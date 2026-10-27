import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";
import { loadEnv } from "vite";
export default defineConfig(({ mode }) => ({
  plugins: [react()],
  test: {
    include: ["src/**/*.test.{ts,tsx}"],
    environment: "jsdom",
    setupFiles: ["./src/test-setup.ts"],
  },
  server: {
    proxy: {
      "/v1":
        loadEnv(mode, ".", "MAGENTS_DEV_").MAGENTS_DEV_API_ORIGIN ??
        "http://127.0.0.1:8787",
      "/auth": "http://127.0.0.1:8787",
    },
  },
}));
