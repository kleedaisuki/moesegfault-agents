import { defineConfig } from "@playwright/test";
import { resolve } from "node:path";
export default defineConfig({
  testDir: "./e2e",
  outputDir: "../../.cache/dashboard-e2e",
  fullyParallel: true,
  workers: 2,
  use: {
    launchOptions: {
      executablePath: process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH,
      downloadsPath: resolve("../../.cache/dashboard-downloads"),
    },
    baseURL: "http://127.0.0.1:5173",
    trace: "retain-on-failure",
  },
  webServer: [
    {
      command: "node e2e/download-server.mjs",
      url: "http://127.0.0.1:5174/healthz",
      reuseExistingServer: false,
    },
    {
      command: "npm run dev -- --port 5173 --strictPort",
      env: { MAGENTS_DEV_API_ORIGIN: "http://127.0.0.1:5174" },
      url: "http://127.0.0.1:5173",
      reuseExistingServer: !process.env.CI,
    },
  ],
});
