// UI side of the cloud opt-in IPC surface (S04): the `set_cloud_optin` /
// `cloud_optin_status` toggle, the presence-only key commands
// (`set_cloud_api_key` / `delete_cloud_api_key` / `cloud_key_status`), the
// persisted heavy-lane provider selection (`set_cloud_heavy_provider` /
// `cloud_heavy_provider`), the `cloud://optin` broadcast, and the pure
// `cloudReducer` behind the Cloud Providers section in Settings. The shapes
// here mirror the serde camelCase serialization of Rust's CloudOptInStatus,
// CloudKeyStatus, CloudHeavyProviderStatus, CloudKeyError, and CloudProvider
// (src-tauri/src/cloud) — a change on either side is a breaking IPC change.
//
// Key material is write-only across this module: `setCloudApiKey` carries a
// key inbound (the one legitimate crossing) and NOTHING here ever returns or
// stores a key — the reducer holds presence booleans only, so the entered key
// can never be echoed back through view state. The reducer is pure, so every
// opt-in/key/provider transition is unit-testable without a Tauri runtime
// (src/cloud-state.test.ts); Settings.tsx is only glue. (Kebab-case name per
// MEM051, matching watcher-state.ts/settings-state.ts.)

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

/** Cloud opt-in broadcast: every opt-in mutation emits the resulting
 *  CloudOptInStatus app-wide, so the ACL-admitted Settings webview stays
 *  truthful whichever surface flipped the toggle. Pinned Rust-side by
 *  `cloud::optin::tests::event_name_is_the_ipc_contract`. */
export const CLOUD_OPTIN_EVENT = "cloud://optin";

/** The cloud providers a key can be stored for — kebab-case over IPC,
 *  mirroring Rust's CloudProvider serde wire name and its `account()`. */
export type CloudProvider = "openai" | "anthropic";

/** Every provider, newest UI order, with its human label. Drives the key
 *  rows and the heavy-lane picker so a new provider is added in one place. */
export const CLOUD_PROVIDERS: readonly { readonly id: CloudProvider; readonly label: string }[] = [
  { id: "openai", label: "OpenAI" },
  { id: "anthropic", label: "Anthropic" },
];

/** Queryable opt-in state (health-as-value, R007): returned by
 *  `cloud_optin_status` / `set_cloud_optin`, broadcast on `cloud://optin`.
 *  `persistError` carries the most recent persist failure so a toggle that
 *  could not be saved stays visible (never an IPC rejection). */
export interface CloudOptInStatus {
  enabled: boolean;
  persistError: string | null;
}

/** Presence-per-provider snapshot — the entire outbound key vocabulary.
 *  Booleans only: no field here ever carries key material (pinned Rust-side
 *  by `status_carries_presence_booleans_only`). */
export interface CloudKeyStatus {
  openaiPresent: boolean;
  anthropicPresent: boolean;
}

/** Queryable heavy-lane provider selection (health-as-value): `provider` is
 *  the kebab-case name or null (unselected); `persistError` carries the most
 *  recent persist failure. Display-only until S05 routes it. */
export interface CloudHeavyProviderStatus {
  provider: CloudProvider | null;
  persistError: string | null;
}

/** A typed key-store failure — the serde kind-tagged serialization of Rust's
 *  CloudKeyError. Details never contain key material. */
export type CloudKeyError =
  | { kind: "invalid-key"; detail: string }
  | { kind: "store-failed"; detail: string };

// ---------------------------------------------------------------------------
// Invoke wrappers
// ---------------------------------------------------------------------------

/** Current cloud opt-in state — health-as-value beside `privacy_status` /
 *  `watcher_status` (R007): a value at any time, never an error. */
export function cloudOptinStatus(): Promise<CloudOptInStatus> {
  return invoke<CloudOptInStatus>("cloud_optin_status");
}

/** Toggle cloud opt-in. Never rejects backend-side: a persist failure comes
 *  back as `persistError` on the resulting status (same contract as
 *  `set_privacy_mode`); rejection outside a Tauri runtime is absorbed by the
 *  caller. */
export function setCloudOptin(enable: boolean): Promise<CloudOptInStatus> {
  return invoke<CloudOptInStatus>("set_cloud_optin", { enable });
}

/** Presence snapshot for the key rows. Rejects with a CloudKeyError when the
 *  OS store fails, and with a plain string/Error outside a Tauri runtime. */
export function cloudKeyStatus(): Promise<CloudKeyStatus> {
  return invoke<CloudKeyStatus>("cloud_key_status");
}

/** Store an API key for a provider — the one legitimate inbound crossing of
 *  key material. Returns the fresh presence snapshot; NEVER returns the key.
 *  Rejects with a CloudKeyError on an empty key or a store failure. */
export function setCloudApiKey(provider: CloudProvider, key: string): Promise<CloudKeyStatus> {
  return invoke<CloudKeyStatus>("set_cloud_api_key", { provider, key });
}

/** Delete a provider's stored key (deleting an absent key succeeds). Returns
 *  the fresh presence snapshot. */
export function deleteCloudApiKey(provider: CloudProvider): Promise<CloudKeyStatus> {
  return invoke<CloudKeyStatus>("delete_cloud_api_key", { provider });
}

/** Current heavy-lane provider selection — health-as-value, never an error. */
export function cloudHeavyProvider(): Promise<CloudHeavyProviderStatus> {
  return invoke<CloudHeavyProviderStatus>("cloud_heavy_provider");
}

/** Set the heavy-lane provider (`null` clears it). Never rejects backend-side:
 *  a persist failure rides `persistError` on the returned status. */
export function setCloudHeavyProvider(
  provider: CloudProvider | null,
): Promise<CloudHeavyProviderStatus> {
  return invoke<CloudHeavyProviderStatus>("set_cloud_heavy_provider", { provider });
}

/** Current coder-lane provider selection (coding-agent S6) — same shape. */
export function cloudCoderProvider(): Promise<CloudHeavyProviderStatus> {
  return invoke<CloudHeavyProviderStatus>("cloud_coder_provider");
}

/** Set the coder-lane provider (`null` clears it) — same contract as the
 *  heavy-lane setter. */
export function setCloudCoderProvider(
  provider: CloudProvider | null,
): Promise<CloudHeavyProviderStatus> {
  return invoke<CloudHeavyProviderStatus>("set_cloud_coder_provider", { provider });
}

/** Subscribe to the app-wide cloud opt-in broadcast (`cloud://optin`). */
export function onCloudOptin(cb: (status: CloudOptInStatus) => void): Promise<UnlistenFn> {
  return listen<CloudOptInStatus>(CLOUD_OPTIN_EVENT, (e) => cb(e.payload));
}

// ---------------------------------------------------------------------------
// Error narrowing
// ---------------------------------------------------------------------------

/** Narrow an invoke rejection to the kind-tagged CloudKeyError contract.
 *  Outside a Tauri runtime (vite dev, Playwright) invoke rejects with a plain
 *  string/Error — that falls through here and the caller treats it as the
 *  no-runtime "unavailable" case, not a store failure. */
export function isCloudKeyError(e: unknown): e is CloudKeyError {
  if (typeof e !== "object" || e === null) return false;
  const kind = (e as { kind?: unknown }).kind;
  return kind === "invalid-key" || kind === "store-failed";
}

// ---------------------------------------------------------------------------
// Cloud Settings state machine (pure)
// ---------------------------------------------------------------------------

export interface CloudViewState {
  /** Live opt-in status; null until the mount-time `cloud_optin_status`
   *  resolves (or forever, outside Tauri — the section renders unavailable). */
  optin: CloudOptInStatus | null;
  /** Per-provider key presence; null until `cloud_key_status` resolves.
   *  Presence booleans only — the entered key never lands here. */
  keys: CloudKeyStatus | null;
  /** Heavy-lane provider selection; null until `cloud_heavy_provider`
   *  resolves. */
  heavy: CloudHeavyProviderStatus | null;
  /** Coder-lane provider selection (S6); null until `cloud_coder_provider`
   *  resolves. */
  coder: CloudHeavyProviderStatus | null;
  /** Most recent key-op failure (empty key, locked store), kept until the
   *  next key status/response supersedes it. Carries a `detail` string,
   *  never a key. */
  keyError: CloudKeyError | null;
}

export const initialCloudViewState: CloudViewState = {
  optin: null,
  keys: null,
  heavy: null,
  coder: null,
  keyError: null,
};

export type CloudViewAction =
  | { type: "optin"; status: CloudOptInStatus }
  | { type: "keys"; status: CloudKeyStatus }
  | { type: "heavy"; status: CloudHeavyProviderStatus }
  | { type: "coder"; status: CloudHeavyProviderStatus }
  | { type: "key-error"; error: CloudKeyError };

export function cloudReducer(state: CloudViewState, action: CloudViewAction): CloudViewState {
  switch (action.type) {
    case "optin":
      // Mount-time query, set_cloud_optin responses, and the cloud://optin
      // broadcast (any window's toggle) all land here — backend authoritative.
      return { ...state, optin: action.status };
    case "keys":
      // A fresh presence snapshot clears any stale key error: the store op it
      // reflects succeeded.
      return { ...state, keys: action.status, keyError: null };
    case "heavy":
      return { ...state, heavy: action.status };
    case "coder":
      return { ...state, coder: action.status };
    case "key-error":
      // A store/validation failure leaves the last presence snapshot intact —
      // the error says what the attempt could not do, without lying about
      // what is currently stored.
      return { ...state, keyError: action.error };
  }
}

// ---------------------------------------------------------------------------
// Copy + selector helpers (pure)
// ---------------------------------------------------------------------------

/** Human label for a provider. */
export function providerLabel(provider: CloudProvider): string {
  return CLOUD_PROVIDERS.find((p) => p.id === provider)?.label ?? provider;
}

/** Whether a key is stored for a provider, from a presence snapshot. */
export function keyPresent(keys: CloudKeyStatus | null, provider: CloudProvider): boolean {
  if (!keys) return false;
  return provider === "openai" ? keys.openaiPresent : keys.anthropicPresent;
}

/** Short human title for a typed key failure. */
export function keyErrorTitle(error: CloudKeyError): string {
  switch (error.kind) {
    case "invalid-key":
      return "That key can't be used";
    case "store-failed":
      return "Key storage failed";
  }
}
