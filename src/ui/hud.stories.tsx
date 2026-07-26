import type { Meta, StoryObj } from "@storybook/react-vite";
import { HudPill } from "./HudPill";
import { ActionTrail } from "./ActionTrail";
import { GhostIndicator } from "./GhostIndicator";

const meta: Meta = { title: "HUD" };
export default meta;

export const PillStates: StoryObj = {
  render: () => (
    <div style={{ display: "flex", flexDirection: "column", gap: 14, alignItems: "flex-start" }}>
      <HudPill tone="acting" headline="click · 312, 208" progress="2 / 3" onStop={() => {}} />
      <HudPill tone="acting" headline="type · “Q3-report-final.pdf”" progress="3 / 3" onStop={() => {}} />
      <HudPill tone="done" headline="Done" />
      <HudPill tone="done" headline="Done — 1 action failed" />
      <HudPill tone="stopped" headline="Stopped — keyboard & mouse are yours" />
    </div>
  ),
};

export const Trail: StoryObj = {
  render: () => (
    <ActionTrail
      items={[
        { id: "c1", label: "read the screen", status: "ok" },
        { id: "c2", label: "focus · TextEdit", status: "ok" },
        { id: "c3", label: "click · 312, 208", status: "failed", failure: "verification-failed" },
        { id: "c4", label: "click · 318, 210", status: "ok" },
        { id: "c5", label: "type · “Q3-report-final.pdf”", status: "running" },
      ]}
    />
  ),
};

export const Ghost: StoryObj = {
  render: () => (
    <div
      style={{
        position: "relative",
        width: 520,
        height: 300,
        background: "var(--te-desktop-gradient)",
        borderRadius: 12,
        overflow: "hidden",
      }}
    >
      <GhostIndicator x={140} y={110} />
      <GhostIndicator x={370} y={200} click />
    </div>
  ),
};
