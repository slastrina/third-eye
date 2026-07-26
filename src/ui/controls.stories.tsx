import { useState } from "react";
import type { Meta, StoryObj } from "@storybook/react-vite";
import { Toggle } from "./Toggle";
import { RadioCards } from "./RadioCard";

const meta: Meta = { title: "Controls" };
export default meta;

function ToggleDemo() {
  const [watching, setWatching] = useState(true);
  const [autoroute, setAutoroute] = useState(false);
  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 14, fontFamily: "var(--te-font)" }}>
      <Toggle on={watching} onChange={setWatching} label={watching ? "Watching is on" : "Watching is off"} />
      <Toggle on={autoroute} onChange={setAutoroute} label="Auto-routing" />
      <Toggle on={true} onChange={() => {}} ariaLabel="Disabled example" disabled />
    </div>
  );
}

export const Toggles: StoryObj = { render: () => <ToggleDemo /> };

function StorageDemo() {
  const [mode, setMode] = useState("text");
  return (
    <div style={{ maxWidth: 420 }}>
      <RadioCards
        label="What is stored"
        value={mode}
        onChange={setMode}
        options={[
          {
            value: "text",
            label: "Distilled text only",
            sub: "Summaries and extracted facts — no pixels are kept",
          },
          {
            value: "thumbs",
            label: "Text + thumbnails",
            sub: "Adds a small screenshot per moment for visual recall",
          },
        ]}
      />
    </div>
  );
}

export const RadioCardsStory: StoryObj = { name: "RadioCards", render: () => <StorageDemo /> };
