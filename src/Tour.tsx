// The first-start tour (2026-07 redesign, surface 1): the light four-step
// wizard card — Welcome → Permissions → Memory → Summon — rendered inside the
// overlay window while first-run is pending. Purely presentational: every
// transition and IPC effect is injected by App.tsx, and all lifecycle logic
// lives in tour-state.ts. Replaces the M006 explainer panel.
import { Button } from "./ui/Button";
import { ChoiceChips } from "./ui/Chip";
import { EyeIcon } from "./ui/EyeIcon";
import { StepIndicator } from "./ui/StepIndicator";
import {
  RETENTION_OPTIONS,
  TOUR_STEPS,
  TOUR_STEP_LABELS,
  shortcutKeycaps,
  tourBlocked,
  tourFinishBlocked,
  tourOnLastStep,
  type Retention,
  type TourViewState,
} from "./tour-state";
import type { PermissionStep } from "./onboarding-state";
import "./tour.css";

export interface TourProps {
  tour: TourViewState;
  /** The live global shortcut ("super+shift+space"); null when unavailable
   *  (outside Tauri) — the Summon step then omits the keycap row. */
  hotkeyShortcut: string | null;
  onNext: () => void;
  onBack: () => void;
  onSkip: () => void;
  onGrantCapture: () => void;
  onGrantInput: () => void;
  onOpenCaptureSettings: () => void;
  onOpenInputSettings: () => void;
  onRetention: (value: Retention) => void;
}

const IS_MAC =
  typeof navigator !== "undefined" && /Mac|iP(hone|ad|od)/.test(navigator.platform);

function PermissionRow({
  title,
  sub,
  step,
  onGrant,
  onOpenSettings,
}: {
  title: string;
  sub: string;
  step: PermissionStep;
  onGrant: () => void;
  onOpenSettings: () => void;
}) {
  return (
    <div className="tour-perm" data-step={step}>
      <div className="tour-perm-text">
        <strong>{title}</strong>
        <span>{sub}</span>
      </div>
      {step === "denied" ? (
        <Button variant="outline" onClick={onOpenSettings}>
          Open System Settings
        </Button>
      ) : step === "granted" ? (
        <span className="tour-perm-granted">Granted ✓</span>
      ) : step === "unsupported" ? (
        <span className="tour-perm-na">Not needed here</span>
      ) : (
        <Button variant="primary" disabled={step === "requesting"} onClick={onGrant}>
          {step === "requesting" ? "Waiting for you…" : "Grant"}
        </Button>
      )}
    </div>
  );
}

export function Tour({
  tour,
  hotkeyShortcut,
  onNext,
  onBack,
  onSkip,
  onGrantCapture,
  onGrantInput,
  onOpenCaptureSettings,
  onOpenInputSettings,
  onRetention,
}: TourProps) {
  const step = TOUR_STEPS[tour.step];
  const blocked = tourBlocked(tour);
  // Skip is only offered when nothing required is missing — a hard block
  // cannot be skipped from any step (M006 posture, enforced again in the
  // caller's finish guard).
  const finishBlocked = tourFinishBlocked(tour);
  const last = tourOnLastStep(tour);
  const keycaps = hotkeyShortcut ? shortcutKeycaps(hotkeyShortcut, IS_MAC) : [];

  return (
    <div className="tour-root">
      <div className="tour-card te-light" role="dialog" aria-labelledby="tour-title">
        <StepIndicator current={tour.step} labels={TOUR_STEP_LABELS} />

        {step === "welcome" && (
          <div className="tour-step tour-step--welcome">
            <EyeIcon state="watching" size={84} stroke="var(--te-light-ink)" />
            <h1 id="tour-title">Meet your Third Eye</h1>
            <p className="tour-sub">
              It watches with you, remembers for you, and acts when you ask.
            </p>
            <ul className="tour-bullets">
              <li>
                <strong>Observes</strong> — distills your screen into moments, on this
                device.
              </li>
              <li>
                <strong>Learns</strong> — builds a knowledgebase of your habits and
                context.
              </li>
              <li>
                <strong>Acts</strong> — summon it with a hotkey; it drives keyboard
                &amp; mouse, visibly.
              </li>
            </ul>
          </div>
        )}

        {step === "permissions" && (
          <div className="tour-step">
            <h1 id="tour-title">Two permissions, both revocable</h1>
            <p className="tour-sub">
              Granted through your OS — Third Eye never works around them.
            </p>
            <PermissionRow
              title="Screen recording"
              sub="So it can observe and attach your screen. Nothing leaves this device."
              step={tour.permissions.capture}
              onGrant={onGrantCapture}
              onOpenSettings={onOpenCaptureSettings}
            />
            <PermissionRow
              title="Input control"
              sub="So it can move the mouse and type — only after you arm it in Settings. Granting now turns nothing on."
              step={tour.permissions.input}
              onGrant={onGrantInput}
              onOpenSettings={onOpenInputSettings}
            />
            {blocked && (
              // R007: the block is visible, never just a disabled button.
              <div className="tour-alert" role="alert">
                <strong>Screen Recording is required</strong>
                <span>
                  Third Eye can't read your screen without it. Grant it above
                  {tour.permissions.capture === "denied"
                    ? " — open System Settings, turn on Third Eye, then come back"
                    : ""}{" "}
                  to continue.
                </span>
              </div>
            )}
          </div>
        )}

        {step === "memory" && (
          <div className="tour-step">
            <h1 id="tour-title">Your memory, your rules</h1>
            <p className="tour-sub">
              Everything stays on this device. You can forget any moment or fact with
              one click.
            </p>
            <div className="tour-seclabel">Keep memory for</div>
            <ChoiceChips
              label="Keep memory for"
              options={RETENTION_OPTIONS}
              value={tour.retention}
              onChange={onRetention}
            />
            <div className="tour-note">
              Pause watching anytime from the tray — the eye closes so you always
              know.
            </div>
          </div>
        )}

        {step === "summon" && (
          <div className="tour-step tour-step--summon">
            <h1 id="tour-title">Summon it anywhere</h1>
            <p className="tour-sub">
              One hotkey opens the palette over whatever you're doing.
            </p>
            {keycaps.length > 0 && (
              <div className="tour-keycaps" aria-label={`Hotkey: ${hotkeyShortcut}`}>
                {keycaps.map((cap, index) => (
                  <span key={`${cap}-${index}`} className="tour-keycap-group">
                    {index > 0 && <span className="tour-keycap-plus">+</span>}
                    <kbd className="tour-keycap">{cap}</kbd>
                  </span>
                ))}
              </div>
            )}
            <p className="tour-hint">Try pressing it now — or click Finish.</p>
          </div>
        )}

        {tour.permissions.persistError && (
          <div className="tour-alert" role="alert">
            <strong>Couldn't save your progress</strong>
            <span>
              {tour.permissions.persistError} — this tour may show again next launch.
            </span>
          </div>
        )}

        <div className="tour-actions">
          {!finishBlocked && (
            <Button variant="ghost" onClick={onSkip}>
              Skip tour
            </Button>
          )}
          <span className="tour-actions-spacer" />
          {tour.step > 0 && (
            <Button variant="outline" onClick={onBack}>
              Back
            </Button>
          )}
          <Button
            variant="primary"
            disabled={blocked}
            title={blocked ? "Grant Screen Recording first" : undefined}
            onClick={onNext}
          >
            {last ? "Finish" : "Continue"}
          </Button>
        </div>
      </div>
    </div>
  );
}
