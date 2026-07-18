// UI side of the tray notice contract defined in src-tauri/src/tray.rs.
// The event name is the contract; keep it in sync with NOTICE_EVENT there.
//
// De-stubbed in S07: Settings…, Configure Models…, and Privacy Mode are real
// surfaces now, so no menu entry maps to a Notice action anymore. The
// plumbing stays for future transient notices — any feature id that does
// arrive still renders a visible, named banner (R010: never silent).

import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export const TRAY_NOTICE_EVENT = "tray://notice";

export interface TrayNoticePayload {
  feature: string;
}

export interface TrayNotice {
  title: string;
  detail: string;
}

/** Map a feature id to banner copy. Every id gets a named fallback — a tray
 *  notice must never fail silently (R010). */
export function noticeMessage(feature: string): TrayNotice {
  return {
    title: feature ? `"${feature}" isn't available yet` : "That isn't available yet",
    detail: "This tray entry isn't wired up in this build.",
  };
}

/** Subscribe to tray notices. Resolves to an unlisten fn. */
export function onTrayNotice(callback: (notice: TrayNotice) => void): Promise<UnlistenFn> {
  return listen<TrayNoticePayload>(TRAY_NOTICE_EVENT, (event) =>
    callback(noticeMessage(event.payload.feature)),
  );
}
