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
// P2-T08 added the `recentRoomsList` and `recentRoomUpsert`
// commands (and the `RecentRoomEntry` and `RecentRoomRole` types).
// P3-T08 added the `downloadState` and `downloadProgress` event
// listeners (`download://state` and `download://progress`) and
// the `DownloadState` / `DownloadStateEvent` /
// `DownloadProgressEvent` types. P3-T12 added the
// `downloadOpen` command (and the `DownloadSessionIpc` return
// type). P4-T02 added the `playbackSend` command (and the
// `PlaybackCommandInput` / `PlaybackSendResult` types) plus the
// `playbackState` event listener (and the `PlaybackStateEvent`
// type). P4-T03 added the `positionReport` command (and the
// `PositionReportInput` / `PositionReportResult` types) plus
// the `positionReport` event listener (and the
// `PositionReportEvent` type). P4-T06 added the
// `clockSkewProbe` command (returning `SkewSample`); the
// 60 s cadence and the 4-sample burst live in the React
// `useClockSkew` hook and the pure-math reducer lives in
// `apps/client/src-tauri/src/room/skew.rs`.

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
  async recentRoomsList(): Promise<RecentRoomEntry[]> {
    return await __TAURI_INVOKE("recent_rooms_list");
  },
  async recentRoomUpsert(entry: RecentRoomEntry): Promise<void> {
    await __TAURI_INVOKE("recent_room_upsert", { entry });
  },
  async downloadOpen(mediaId: string): Promise<DownloadSessionIpc> {
    return await __TAURI_INVOKE("download_open", { mediaId });
  },
  // P4-T02: host playback command send. Wraps the
  // PLAYBACK_CMD envelope in the host process and
  // forwards it through the signaling WebSocket.
  async playbackSend(cmd: PlaybackCommandInput): Promise<PlaybackSendResult> {
    return await __TAURI_INVOKE("playback_send", { cmd });
  },
  // P4-T03: 1 Hz POSITION_REPORT send. Wraps the
  // POSITION_REPORT envelope in the host process and
  // forwards it through the signaling WebSocket. The
  // server is a pure relay and broadcasts the report to
  // every other participant in the room. The cadence is
  // owned by the React layer (see
  // apps/client/src/components/Player.tsx); this
  // command is a single-shot fire-and-forget call.
  async positionReport(
    report: PositionReportInput,
  ): Promise<PositionReportResult> {
    return await __TAURI_INVOKE("position_report", { report });
  },
  // P4-T06: NTP-style clock skew probe. Returns a single
  // `SkewSample` (t0 / t3 / server_ts_ms / echoed
  // client_send_ms). The React layer owns the 60 s
  // cadence and the 4-sample burst, and the
  // `useClockSkew` hook reduces the samples into
  // (skewMs, jitterMs) via the same math as
  // `apps/client/src-tauri/src/room/skew.rs::compute_skew_jitter`.
  async clockSkewProbe(): Promise<SkewSample> {
    return await __TAURI_INVOKE<SkewSample>("clock_skew_probe");
  },
  // P5-T02: per-stroke drawing envelope send. Wraps the
  // DRAW_BEGIN / DRAW_POINT / DRAW_END envelope in the
  // host process (the DRAW_BEGIN payload is signed
  // server-side because the Ed25519 private key never
  // leaves the Rust keyring) and forwards the envelope
  // through the signaling WebSocket. The `input` shape
  // is the discriminated union declared in
  // `DrawingSendInput` (Tauri command); see
  // apps/client/src-tauri/src/commands/drawing.rs.
  async drawingSend(input: DrawingSendInput): Promise<DrawingSendResult> {
    return await __TAURI_INVOKE<DrawingSendResult>("drawing_send", { input });
  },
};

// P5-T02: typed shape for `drawing_send`. Mirrors the
// Rust `DrawingSendInput` enum (Begin / Point / End
// variants discriminated by `action`).
export type DrawingSendInput =
  | {
      action: "begin";
      stroke_id: string;
      tool: string;
      color: string;
      width: number;
      x: number;
      y: number;
      pressure: number;
      ts_ms: number;
      client_seq: number;
    }
  | {
      action: "point";
      stroke_id: string;
      x: number;
      y: number;
      pressure: number;
      ts_ms: number;
      client_seq: number;
    }
  | {
      action: "end";
      stroke_id: string;
      ts_ms: number;
      client_seq: number;
    };

export interface DrawingSendResult {
  envelope_id: string;
  stroke_id: string;
}

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

export type RecentRoomRole = "host" | "guest";

export type RecentRoomEntry = {
  room_id: string;
  code: string;
  title: string;
  host_user_id: string;
  host_display_name: string;
  role: RecentRoomRole;
  last_seen_ms: number;
  last_ended_ms: number | null;
  created_ms: number;
};

// P3-T08: download progress + state event payload types.
// The wire format is hand-maintained to mirror the Rust
// `DownloadStateEvent` / `DownloadProgressEvent` shapes in
// `apps/client/src-tauri/src/transfer/events.rs`; tauri-specta
// does not generate them (the events are emitted via
// `AppHandle::emit`, not registered via `collect_events!`).
export type DownloadState =
  | "pending"
  | "connecting"
  | "transferring"
  | "verifying"
  | "complete"
  | "failed"
  | "paused"
  | "cancelled";

export type DownloadStateEvent = {
  v: 1;
  id: string;
  media_id: string;
  state: DownloadState;
  error_message: string | null;
};

export type DownloadProgressEvent = {
  v: 1;
  id: string;
  state: DownloadState;
  transferred_bytes: number;
  total_bytes: number;
  bytes_per_sec_ema: number;
  eta_seconds: number | null;
};

// P3-T12: return type for the `downloadOpen` command.
    // P4-T02: the host playback command send + the
    // `playback://state` event payload.
    export type PlaybackCommandInput = {
        action: "play" | "pause" | "seek";
        monotonic_seq: number;
        media_position_ms: number;
    };

    export type PlaybackSendResult = {
        envelope_id: string;
        monotonic_seq: number;
    };

    export type PlaybackStateEvent = {
        room_id: string;
        server_seq: number;
        server_ts_ms: number;
        sender_id: string;
        monotonic_seq: number;
        kind: "play" | "pause" | "seek";
        media_position_ms: number;
    };

    // P4-T03: 1 Hz POSITION_REPORT input. The local
    // <video> element's currentTime is converted to
    // integer milliseconds by the React layer before
    // calling `commands.positionReport`. The server is a
    // pure relay (architecture section 12.8 + roadmap
    // P4-T03 "server forwards without modification").
    export type PositionReportInput = {
        media_position_ms: number;
        playing: boolean;
    };

    export type PositionReportResult = {
        envelope_id: string;
    };

    // P4-T03: the `position://report` event payload. The
    // server forwards the wire payload verbatim and stamps
    // `sender_id` (= the originator's user_id) so the
    // React layer can key positions by sender and keep
    // multiple viewers distinguishable.
    export type PositionReportEvent = {
        room_id: string;
        sender_id: string;
        media_position_ms: number;
        playing: boolean;
        client_ts_ms: number;
    };

// P5-T03: the `drawing://begin` event payload. Emitted
// when a remote DRAW_BEGIN is accepted and rebroadcast by
// the server. The sender_id is the server-authoritative
// originator (from the validated bearer).
export type StrokeBeginEvent = {
    room_id: string;
    sender_id: string;
    stroke_id: string;
    tool: string;
    color: string;
    width: number;
    x: number;
    y: number;
    pressure: number;
    ts_ms: number;
};

// P5-T03: the `drawing://point` event payload. Emitted
// when a remote DRAW_POINT is accepted and rebroadcast by
// the server.
export type StrokePointEvent = {
    room_id: string;
    sender_id: string;
    stroke_id: string;
    x: number;
    y: number;
    pressure: number;
    ts_ms: number;
};

// P5-T03: the `drawing://end` event payload. Emitted
// when a remote DRAW_END is accepted and rebroadcast by
// the server.
export type StrokeEndEvent = {
    room_id: string;
    sender_id: string;
    stroke_id: string;
    ts_ms: number;
};
export type DownloadSessionIpc = {
  download_id: string;
  media_id: string;
  state: string;
  dedup_hit: boolean;
  total_bytes: number;
  transferred_bytes: number;
  on_disk_path: string | null;
};

// P4-T06: the SKEW_PROBE round-trip's 4-timestamp sample
// (architecture section 13.3). The Rust side emits this from
// `RoomClient::clock_skew_probe`; the React layer reduces a
// burst of 4 samples into (skewMs, jitterMs) and stores them
// in the `useClockSkewStore`. The 60 s cadence is owned by
// the React `useClockSkew` hook.
export type SkewSample = {
    t0_local_ms: number;
    t3_local_ms: number;
    server_ts_ms: number;
    client_send_ms_echo: number;
};

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
  downloadState: <EventListener<DownloadStateEvent>>((h) =>
    __listenAs__("download://state", h)
  ),
  downloadProgress: <EventListener<DownloadProgressEvent>>((h) =>
    __listenAs__("download://progress", h)
  ),
  // P4-T02: server-authoritative playback state. Emitted
  // every time the server accepts a host PLAYBACK_CMD
  // (PLAY / PAUSE / SEEK) and rebroadcasts it. The
  // `server_seq` is monotonic per room; the React
  // `usePlaybackStore` drops events with
  // `server_seq <= lastAppliedServerSeq` for the same
  // room and buffers the most recent event for the
  // case where the <video> element is not yet ready.
  playbackState: <EventListener<PlaybackStateEvent>>((h) =>
    __listenAs__("playback://state", h)
  ),
  // P4-T03: inbound POSITION_REPORT from a remote
  // participant. Emitted ~1 Hz per remote viewer /
  // host while the user is in a room. The local
  // 1 Hz reporter (Player.tsx) does NOT consume this
  // event; it only drives outbound reports. Receiving
  // a report never produces another outbound report
  // (no feedback loop). The receiving client is
  // typically the host, who uses the per-sender
  // position to render a "viewer's position"
  // indicator.
  positionReport: <EventListener<PositionReportEvent>>((h) =>
    __listenAs__("position://report", h)
  ),
  // P5-T03: remote DRAW_BEGIN rebroadcast. The server
  // accepts a signed DRAW_BEGIN, binds stroke_id to the
  // sender, and rebroadcasts to other room participants.
  // The local client uses this to create a remote stroke.
  strokeBegin: <EventListener<StrokeBeginEvent>>((h) =>
    __listenAs__("drawing://begin", h)
  ),
  // P5-T03: remote DRAW_POINT rebroadcast. Appends to
  // the remote stroke identified by stroke_id.
  strokePoint: <EventListener<StrokePointEvent>>((h) =>
    __listenAs__("drawing://point", h)
  ),
  // P5-T03: remote DRAW_END rebroadcast. Finalizes
  // the remote stroke.
  strokeEnd: <EventListener<StrokeEndEvent>>((h) =>
    __listenAs__("drawing://end", h)
  ),
};

export const signalingStateChanged = events.signalingState;
export const roomStateChanged = events.roomState;
export const roomEventEnvelope = events.roomEvent;
export const downloadStateChanged = events.downloadState;
export const downloadProgressChanged = events.downloadProgress;
export const playbackStateChanged = events.playbackState;
export const positionReportChanged = events.positionReport;
export const strokeBeginChanged = events.strokeBegin;
export const strokePointChanged = events.strokePoint;
export const strokeEndChanged = events.strokeEnd;
