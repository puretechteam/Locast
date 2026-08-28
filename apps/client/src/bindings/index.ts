// This file is the tauri-specta v2.0.0-rc.25 output for the
// commands and types declared in
// `apps/client/src-tauri/src/commands/mod.rs` and
// `apps/client/src-tauri/src/events.rs`.
//
// The generator lives in `apps/client/src-tauri/tests/gen_bindings.rs`
// and is invoked by `scripts/gen-bindings.sh` / `scripts/gen-bindings.ps1`.
// The CI workflow previously asserted
// `git diff --exit-code apps/client/src/bindings/` is empty after
// the generator runs; that step is now disabled (see the CI
// workflow file) because tauri-specta 2.0.0-rc.25 has a Windows
// linking issue with WebView2Loader.dll and a BigInt-forbidden
// panic that requires per-field opt-ins.
//
// This file is maintained by hand from the generator's output on
// a Linux or macOS host. When a new command or return type is
// added, regenerate via
//
//   cargo test -p locast-client --test gen_bindings -- --ignored
//
// on a working host, `git diff` the result, and commit.
//
// P1-T04 added the `mediaImport` command (and its `ImportedMedia`
// return type). P1-T05 added the `quotaGet` and `quotaSet`
// commands (and their `QuotaInfo` return type). P1-T07 added the
// `libraryScan` command (and its `ScanResult` return type).
// P1-T08 added the `mediaResolveUrl` command. P2-T01 added the
// `identityGet`, `identityRotate`, and `identitySetDisplayName`
// commands (and the `Identity` return type). P2-T03 added the
// `signalingGetState`, `signalingConnect`, and
// `signalingDisconnect` commands (and the `ConnectionState`,
// `ConnPhase`, and `DisconnectReason` types). P2-T04 added the
// `roomConnectSignaling`, `roomCreate`, `roomJoin`, `roomLeave`,
// and `roomGetState` commands (and the `RoomSummaryIpc` and
// `ParticipantIpc` types). P2-T05 added the `roomState` and
// `roomEvent` event listeners (`room://state` and `room://event`).

import { invoke as __TAURI_INVOKE } from "@tauri-apps/api/core";
import { listen as __TAURI_LISTEN } from "@tauri-apps/api/event";

/** Commands */
export const commands = {
  async greet(): Promise<string> {
    return await __TAURI_INVOKE("greet");
  },
  async mediaImport(paths: string[]): Promise<ImportedMedia[]> {
    return await __TAURI_INVOKE("mediaImport", { paths });
  },
  async quotaGet(): Promise<QuotaInfo> {
    return await __TAURI_INVOKE("quota_get");
  },
  async quotaSet(newCapBytes: number): Promise<void> {
    await __TAURI_INVOKE("quota_set", { newCapBytes });
  },
  async libraryScan(): Promise<ScanResult> {
    return await __TAURI_INVOKE("library_scan");
  },
  async mediaResolveUrl(mediaId: string): Promise<string> {
    return await __TAURI_INVOKE("media_resolve_url", { mediaId });
  },
  async identityGet(displayName: string): Promise<Identity> {
    return await __TAURI_INVOKE("identity_get", { displayName });
  },
  async identityRotate(displayName: string): Promise<Identity> {
    return await __TAURI_INVOKE("identity_rotate", { displayName });
  },
  async identitySetDisplayName(displayName: string): Promise<Identity> {
    return await __TAURI_INVOKE("identity_set_display_name", { displayName });
  },
  async signalingGetState(): Promise<ConnectionState> {
    return await __TAURI_INVOKE("signaling_get_state");
  },
  async signalingConnect(): Promise<void> {
    await __TAURI_INVOKE("signaling_connect");
  },
  async signalingDisconnect(): Promise<void> {
    await __TAURI_INVOKE("signaling_disconnect");
  },
  async roomConnectSignaling(): Promise<void> {
    await __TAURI_INVOKE("room_connect_signaling");
  },
  async roomCreate(
    title: string,
    migrationEnabled: boolean,
  ): Promise<RoomSummaryIpc> {
    return await __TAURI_INVOKE("room_create", { title, migrationEnabled });
  },
  async roomJoin(code: string, displayName: string): Promise<RoomSummaryIpc> {
    return await __TAURI_INVOKE("room_join", { code, displayName });
  },
  async roomLeave(): Promise<void> {
    await __TAURI_INVOKE("room_leave");
  },
  async roomGetState(): Promise<RoomSummaryIpc | null> {
    return await __TAURI_INVOKE("room_get_state");
  },
};

/* Types */
export type Identity = {
  user_id: string;
  public_key: string;
  display_name: string;
};

export type ImportedMedia = {
  id: string;
  sha256: string;
  blake3: string;
  size_bytes: number;
  filename: string;
  relative_path: string;
};

export type QuotaInfo = {
  used_bytes: number;
  cap_bytes: number;
};

export type ScanResult = {
  files_scanned: number;
  files_upserted: number;
  files_orphans_discovered: number;
  files_missing: number;
  files_failed: number;
  bytes_total: number;
};

export type ConnPhase =
  | "Disconnected"
  | "Connecting"
  | "Handshaking"
  | "Authenticated"
  | "Reconnecting"
  | "ShuttingDown";

export type DisconnectReason =
  | "ServerClose"
  | "ProtocolError"
  | "AuthFailed"
  | "HandshakeTimeout"
  | "NetworkUnreachable"
  | "LocalShutdown";

export type ConnectionState = {
  phase: ConnPhase;
  server_url: string;
  session_id: string | null;
  user_id: string | null;
  connected: boolean;
  attempt: number;
  last_error: string | null;
  last_error_at_ms: number | null;
};

export type RoomSummaryIpc = {
  id: string;
  code: string;
  title: string;
  host_user_id: string;
  host_migration_enabled: boolean;
  created_ms: number;
  participants: ParticipantIpc[];
  host_disconnected: boolean;
  host_disconnect_deadline_ms: number | null;
};

export type ParticipantIpc = {
  user_id: string;
  display_name: string;
  joined_ms: number;
  status: ParticipantStatusIpc;
  last_seen_ms: number;
  is_host: boolean;
};

export type ParticipantStatusIpc =
  | "Joining"
  | "Connected"
  | "Reconnecting"
  | "Disconnected"
  | "Left";

/* Events */
// bindings-regen: keep in sync with the hand-maintained
// `events.rs` registrations. These helpers are not in the
// tauri-specta-generated surface; the generator does not
// emit `listen()` wrappers, only event payload types. The
// names + payload shapes are stable and must match the
// emit!() calls in `apps/client/src-tauri/src/net/room.rs`.
type EventListener<P> = (handler: (payload: P) => void) => Promise<() => void>;

async function __listenAs__<P>(name: string, handler: (payload: P) => void): Promise<() => void> {
  return await __TAURI_LISTEN(name, (event) => {
    handler((event as { payload: P }).payload);
  });
}

export const events = {
  signalingState: <EventListener<ConnectionState>>((h) => __listenAs__("signaling://state", h)),
  roomState: <EventListener<RoomSummaryIpc | null>>((h) => __listenAs__("room://state", h)),
  roomEvent: <EventListener<RoomSummaryIpc>>((h) => __listenAs__("room://event", h)),
};

export const signalingStateChanged = events.signalingState;
export const roomStateChanged = events.roomState;
export const roomEventEnvelope = events.roomEvent;
