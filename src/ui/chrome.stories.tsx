import type { Meta, StoryObj } from "@storybook/react-vite";
import { EyeIcon } from "./EyeIcon";
import { StepIndicator } from "./StepIndicator";
import { Toast } from "./Toast";
import { Panel } from "./Panel";

const meta: Meta = { title: "Chrome" };
export default meta;

export const EyeStates: StoryObj = {
  render: () => (
    <div style={{ display: "flex", gap: 28, alignItems: "center", color: "#fff", fontFamily: "var(--te-font)" }}>
      {(["watching", "thinking", "acting", "closed"] as const).map((state) => (
        <div key={state} style={{ textAlign: "center", fontSize: 11, color: "var(--te-text-dim)" }}>
          <EyeIcon state={state} size={48} />
          <div>{state}</div>
        </div>
      ))}
    </div>
  ),
};

export const EyeOnLight: StoryObj = {
  globals: { backgrounds: { value: "light" } },
  render: () => (
    <div className="te-light" style={{ color: "var(--te-light-ink)", background: "#fff", padding: 24, borderRadius: 16 }}>
      <EyeIcon state="watching" size={84} />
    </div>
  ),
};

export const Steps: StoryObj = {
  globals: { backgrounds: { value: "light" } },
  render: () => (
    <div className="te-light" style={{ background: "#fff", padding: 24, borderRadius: 16, width: 480 }}>
      {[0, 1, 3].map((current) => (
        <div key={current} style={{ marginBottom: 24 }}>
          <StepIndicator current={current} labels={["Welcome", "Permissions", "Memory", "Summon"]} />
        </div>
      ))}
    </div>
  ),
};

export const Toasts: StoryObj = {
  render: () => (
    <div style={{ display: "flex", flexDirection: "column", gap: 12, alignItems: "flex-start" }}>
      <Toast placement="inline">Third Eye is watching — look for the eye in your tray</Toast>
      <Toast placement="inline" tone="red">
        Stopped — keyboard &amp; mouse are yours
      </Toast>
    </div>
  ),
};

export const Panels: StoryObj = {
  render: () => (
    <div style={{ display: "flex", gap: 16, flexWrap: "wrap" }}>
      <Panel variant="glass" accent="green" style={{ width: 260, padding: 18 }}>
        glass · green accent (palette)
      </Panel>
      <Panel variant="glass" accent="amber" style={{ width: 260, padding: 18 }}>
        glass · amber accent (HUD, acting)
      </Panel>
      <Panel variant="solid" style={{ width: 260, padding: 18 }}>
        solid (memory, settings)
      </Panel>
    </div>
  ),
};
