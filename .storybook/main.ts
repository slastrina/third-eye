import type { StorybookConfig } from "@storybook/react-vite";

// Storybook is a dev-only workbench for the src/ui design system. It shares
// the app's Vite pipeline but is never part of the Tauri bundle (`vite build`
// and `storybook build` are separate outputs; only dist/ ships).
const config: StorybookConfig = {
  framework: "@storybook/react-vite",
  stories: ["../src/ui/**/*.stories.tsx"],
};

export default config;
