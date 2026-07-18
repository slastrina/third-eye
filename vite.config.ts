/// <reference types="vitest/config" />
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri expects a fixed dev port; fail loudly if it is taken.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
  },
  envPrefix: ["VITE_", "TAURI_"],
  test: {
    // Playwright owns e2e/*.spec.ts; keep vitest on the unit tests only.
    include: ["src/**/*.test.{ts,tsx}"],
  },
  build: {
    target: "safari15",
    minify: "esbuild",
    sourcemap: false,
  },
});
