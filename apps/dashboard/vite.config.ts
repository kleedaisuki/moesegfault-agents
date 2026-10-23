import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";
export default defineConfig({
  plugins: [react()],
  test: {
    include: ["src/**/*.test.{ts,tsx}"],
    environment: "jsdom",
    setupFiles: ["./src/test-setup.ts"],
  },
  server: {
    proxy: { "/v1": "http://127.0.0.1:8787", "/auth": "http://127.0.0.1:8787" },
  },
});
