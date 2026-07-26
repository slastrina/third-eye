import { useState } from "react";
import type { Meta, StoryObj } from "@storybook/react-vite";
import { Button } from "./Button";
import { Chip, ChoiceChips } from "./Chip";
import { SectionLabel } from "./SectionLabel";

const meta: Meta = { title: "Primitives" };
export default meta;

const row: React.CSSProperties = {
  display: "flex",
  gap: 10,
  alignItems: "center",
  flexWrap: "wrap",
  fontFamily: "var(--te-font)",
};

export const Buttons: StoryObj = {
  render: () => (
    <div style={{ display: "flex", flexDirection: "column", gap: 16 }}>
      <div style={row}>
        <Button variant="primary">Continue</Button>
        <Button variant="outline">Back</Button>
        <Button variant="accent">Summon ⌥␣</Button>
        <Button variant="danger">Stop · esc</Button>
        <Button variant="ghost">Skip tour</Button>
      </div>
      <div style={row}>
        <Button variant="primary" disabled>
          Continue
        </Button>
        <Button variant="outline" disabled>
          Back
        </Button>
      </div>
    </div>
  ),
};

export const ButtonsOnLight: StoryObj = {
  globals: { backgrounds: { value: "light" } },
  render: () => (
    <div className="te-light" style={{ ...row, background: "var(--te-light-surface)", padding: 24, borderRadius: 16 }}>
      <Button variant="primary">Continue</Button>
      <Button variant="outline">Back</Button>
      <Button variant="ghost">Skip tour</Button>
    </div>
  ),
};

export const Chips: StoryObj = {
  render: () => (
    <div style={{ display: "flex", flexDirection: "column", gap: 16 }}>
      <div style={row}>
        <Chip>Zed — third_eye/src/watcher.rs</Chip>
        <Chip tone="selected">● Screen attached</Chip>
        <Chip tone="amber">Deleting files</Chip>
        <Chip tone="dashed" onClick={() => {}}>
          + Add app
        </Chip>
        <Chip onRemove={() => {}} removeLabel="Remove 1Password">
          1Password
        </Chip>
      </div>
    </div>
  ),
};

function RetentionDemo() {
  const [value, setValue] = useState("30d");
  return (
    <ChoiceChips
      label="Keep memory for"
      options={[
        { value: "7d", label: "7 days" },
        { value: "30d", label: "30 days" },
        { value: "90d", label: "90 days" },
        { value: "forever", label: "Forever" },
      ]}
      value={value}
      onChange={setValue}
    />
  );
}

export const ChoiceChipsStory: StoryObj = {
  name: "ChoiceChips",
  render: () => <RetentionDemo />,
};

export const SectionLabels: StoryObj = {
  render: () => (
    <div>
      <SectionLabel>Pause watching</SectionLabel>
      <SectionLabel>From your memory</SectionLabel>
    </div>
  ),
};
