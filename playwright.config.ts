import { defineConfig, devices } from "@playwright/test";

// Browser-executable UAT runs against the plain vite dev server (no Tauri
// runtime), mirroring the slice UAT precondition: npm run dev → :1420.
export default defineConfig({
  testDir: "e2e",
  timeout: 30_000,
  fullyParallel: true,
  forbidOnly: !!process.env.CI,
  retries: 0,
  reporter: "list",
  use: {
    baseURL: "http://localhost:1420",
    trace: "retain-on-failure",
  },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
  webServer: {
    command: "npm run dev",
    url: "http://localhost:1420",
    // vite uses strictPort on 1420, so a second server can never start;
    // reusing a running dev server is the only way concurrent runs work.
    reuseExistingServer: true,
    timeout: 60_000,
  },
});
