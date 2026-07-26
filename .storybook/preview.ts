import type { Preview } from "@storybook/react-vite";

// The same stylesheets the app loads, so stories render on real tokens.
// Stories that need the navy desktop backdrop pick the `navy` background;
// light-surface components (tour card) use `light`.
import "../src/ui/tokens.css";

const preview: Preview = {
  parameters: {
    backgrounds: {
      options: {
        navy: { name: "navy", value: "#071D49" },
        light: { name: "light", value: "#ECECF1" },
      },
    },
  },
  initialGlobals: {
    backgrounds: { value: "navy" },
  },
};

export default preview;
