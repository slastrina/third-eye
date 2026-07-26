// Token reference sheet — the design-review surface for the raw palette.
// Doubles as the A1 smoke story: if this renders, Storybook + tokens work.
import type { Meta, StoryObj } from "@storybook/react-vite";

const COLOR_TOKENS = [
  "--te-navy-deep",
  "--te-navy",
  "--te-navy-panel",
  "--te-navy-raised",
  "--te-green",
  "--te-green-hover",
  "--te-green-deep",
  "--te-green-ink",
  "--te-amber",
  "--te-red",
  "--te-red-soft",
  "--te-light-surface",
  "--te-light-bg",
  "--te-light-ink",
  "--te-light-body",
  "--te-light-muted",
];

function Swatches() {
  return (
    <div
      style={{
        display: "grid",
        gridTemplateColumns: "repeat(4, 150px)",
        gap: 12,
        fontFamily: "var(--te-font)",
      }}
    >
      {COLOR_TOKENS.map((token) => (
        <div key={token} style={{ color: "var(--te-text)", fontSize: 11 }}>
          <div
            style={{
              height: 56,
              borderRadius: "var(--te-r-md)",
              background: `var(${token})`,
              border: "1px solid var(--te-line-strong)",
              marginBottom: 6,
            }}
          />
          {token}
        </div>
      ))}
    </div>
  );
}

const meta: Meta<typeof Swatches> = {
  title: "Tokens/Colors",
  component: Swatches,
};
export default meta;

export const Colors: StoryObj<typeof Swatches> = {};
