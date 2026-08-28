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
// `ConnPhase`, and `DisconnectReason` types).

import { invoke as __TAURI_INVOKE } from "@tauri-apps/api/core";

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
