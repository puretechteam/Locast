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
// `apps/client/src-tauri/src/room/skew.rs`. P1-T09 added the
// `libraryList`, `libraryMakePermanent`, and `libraryDelete`
// commands (and the `LibraryItem` type) and corrected the
// `mediaImport` invoke name to the Rust command's `media_import`.
// Slice 3 (host share / viewer download) added the
// `manifestPublish`, `manifestFetch`, `manifestCurrent`, and
// `roomInviteUrl` commands (and the `SharedManifestIpc` /
// `SharedMediaIpc` / `ManifestResponsePayload` types), the
// `inviteUrl` argument of `roomJoin`, `you_user_id` on
// `RoomSummaryIpc`, `transfer_started` on `DownloadSessionIpc`, and
// the `manifestState` event listener (`manifest://state`).
// P5-T04 added the `laserSend` command (and the `LaserSendInput` /
// `LaserSendResult` types) plus the `laserMove` / `laserOff` event
// listeners (`laser://move` / `laser://off`, payloads
// `LaserMoveEvent` / `LaserOffEvent`).
// `cap_set` on `ParticipantIpc` (serialized since P6-T02; known
// only after a CAPABILITY_UPDATE, room snapshots carry 0).

import { invoke as __TAURI_INVOKE } from "@tauri-apps/api/core";
import { listen as __TAURI_LISTEN } from "@tauri-apps/api/event";

/** Commands */
export const commands = {
  async greet(): Promise<string> {
    return await __TAURI_INVOKE("greet");
  },
  async mediaImport(paths: string[]): Promise<ImportedMedia[]> {
    return await __TAURI_INVOKE("media_import", { paths });
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
  async libraryList(
    query: string | null,
    limit: number | null,
    offset: number | null,
  ): Promise<LibraryItem[]> {
    return await __TAURI_INVOKE("library_list", { query, limit, offset });
  },
  async libraryMakePermanent(id: string): Promise<void> {
    await __TAURI_INVOKE("library_make_permanent", { id });
  },
  async libraryDelete(id: string): Promise<void> {
    await __TAURI_INVOKE("library_delete", { id });
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
  // `inviteUrl` is the host's `locast://join/<code>?h=<key>&v=1`
  // link; its `h=` key is the manifest trust anchor. Without it
  // the viewer can join but every manifest is rejected.
  async roomJoin(
    code: string,
    displayName: string,
    inviteUrl: string | null = null,
  ): Promise<RoomSummaryIpc> {
    return await __TAURI_INVOKE("room_join", { code, displayName, inviteUrl });
  },
  // Host only: the invite link carrying the host's public key.
  async roomInviteUrl(): Promise<string> {
    return await __TAURI_INVOKE("room_invite_url");
  },
  // Host only (server-enforced): sign and publish a manifest for
  // the chosen library items (`null` = every permanent item).
  async manifestPublish(mediaIds: string[] | null): Promise<void> {
    await __TAURI_INVOKE("manifest_publish", { mediaIds });
  },
  // Late-join fetch of the room's current manifest. The result is
  // accepted only after signature + trust-anchor checks in Rust.
  // `mediaId` is informational (the server returns the latest).
  async manifestFetch(mediaId: string): Promise<ManifestResponsePayload> {
    return await __TAURI_INVOKE("manifest_fetch", { mediaId });
  },
  // The verified manifest cached for the current room, if any.
  async manifestCurrent(): Promise<SharedManifestIpc | null> {
    return await __TAURI_INVOKE("manifest_current");
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
  // P6-T02: grant or revoke capabilities for a participant.
  // The caller must be the room host; the server enforces this.
  async roomPermissionSet(
    targetUserId: string,
    addCapSet: number,
    removeCapSet: number,
  ): Promise<void> {
    await __TAURI_INVOKE("room_permission_set", {
      targetUserId,
      addCapSet,
      removeCapSet,
    });
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
  // P5-T04: send one LASER_MOVE / LASER_OFF to the current room.
  // Unsigned; the server stamps the authenticated sender on the
  // relay. The React layer throttles to <= 60 Hz
  // (src/laser/laserTransport.ts); see
  // apps/client/src-tauri/src/commands/laser.rs.
  async laserSend(input: LaserSendInput): Promise<LaserSendResult> {
    return await __TAURI_INVOKE<LaserSendResult>("laser_send", { input });
  },
  // P6-T03: send a chat message. The caller passes
  // the text (max 2 KiB) and an optional reply_to
  // message id. The server validates the CHAT cap
  // and rebroadcasts the message to all participants.
  async roomChatMessage(text: string, replyTo: string | null): Promise<void> {
    return await __TAURI_INVOKE("room_chat_message", { text, replyTo });
  },
  // P6-T06: fetch the list of temp files for a room.
  async getTempFiles(roomId: string): Promise<TempFileInfo[]> {
    return await __TAURI_INVOKE("get_temp_files", { roomId });
  },
  // P6-T06: mark temp files as permanent.
  async markFilesPermanent(fileIds: string[]): Promise<void> {
    return await __TAURI_INVOKE("mark_files_permanent", { fileIds });
  },
  // P6-T06: move temp files to trash.
  async deleteFilesToTrash(fileIds: string[]): Promise<void> {
    return await __TAURI_INVOKE("delete_files_to_trash", { fileIds });
  },
};

// P5-T02: typed shape for `drawing_send`. Mirrors the
// Rust `DrawingSendInput` enum (Begin / Point / End
// variants discriminated by `action`). P5-T03 adds `undo`
// (one stroke id) and `clear` (no fields).
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
    }
  | {
      action: "undo";
      stroke_id: string;
    }
  | {
      action: "clear";
    };

export interface DrawingSendResult {
  envelope_id: string;
  // `null` for a `clear` (it concerns no single stroke).
  stroke_id: string | null;
}

// P5-T04: typed shape for `laser_send`. Mirrors the Rust
// `LaserSendInput` enum (Move / Off discriminated by `action`).
// `x` / `y` are normalized to the video frame and must be finite
// and within [0, 1]; the command rejects anything else.
export type LaserSendInput =
  | {
      action: "move";
      x: number;
      y: number;
    }
  | {
      action: "off";
    };

export interface LaserSendResult {
  envelope_id: string;
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

export type LibraryItem = {
  id: string;
  sha256: string;
  filename: string;
  size_bytes: number;
  /** Probe-derived; `None` when ffprobe was unavailable at import. */
  duration_ms: number | null;
  width: number | null;
  height: number | null;
  video_codec: string | null;
  audio_codec: string | null;
  container: string | null;
  /** `"permanent"` or `"temporary"` (the schema's CHECK constraint). */
  status: string;
  /** Unix milliseconds. */
  created_at: number;
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
  you_cap_set?: number;
  you_user_id?: string | null;
};

export type ParticipantIpc = {
  user_id: string;
  display_name: string;
  joined_ms: number;
  status: ParticipantStatusIpc;
  last_seen_ms: number;
  is_host: boolean;
  cap_set: number;
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
    /** The room's drawing sequence number (`Envelope::seq`). An
     *  event at or below the last one applied is ignored. */
    seq?: number;
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
    /** The room's drawing sequence number (`Envelope::seq`). An
     *  event at or below the last one applied is ignored. */
    seq?: number;
};

// P5-T03: the `drawing://end` event payload. Emitted
// when a remote DRAW_END is accepted and rebroadcast by
// the server.
export type StrokeEndEvent = {
    room_id: string;
    sender_id: string;
    stroke_id: string;
    ts_ms: number;
    /** The room's drawing sequence number (`Envelope::seq`). An
     *  event at or below the last one applied is ignored. */
    seq?: number;
};

// P5-T03: the `drawing://undo` event payload. Emitted when the
// server accepts a DRAW_UNDO, to EVERY participant including the
// actor. `sender_id` is the server-stamped actor (who undid), not
// the stroke's owner.
export type StrokeUndoEvent = {
    room_id: string;
    sender_id: string;
    stroke_id: string;
    /** The room's drawing sequence number (`Envelope::seq`). An
     *  event at or below the last one applied is ignored. */
    seq?: number;
};

// P5-T03: the `drawing://clear` event payload. Emitted when the
// server accepts a DRAW_CLEAR, to every participant including the
// actor. `sender_id` is the server-stamped actor.
export type StrokeClearEvent = {
    room_id: string;
    sender_id: string;
    /** The room's drawing sequence number (`Envelope::seq`). An
     *  event at or below the last one applied is ignored. */
    seq?: number;
};

// P5-T04: the `laser://move` event payload. Emitted when the server
// relays another participant's LASER_MOVE. `sender_id` is the
// server-stamped sender (never read from the wire payload). The
// local user's own laser is never delivered.
export type LaserMoveEvent = {
    room_id: string;
    sender_id: string;
    x: number;
    y: number;
};

// P5-T04: the `laser://off` event payload. Emitted when the server
// relays another participant's LASER_OFF (they released the laser).
export type LaserOffEvent = {
    room_id: string;
    sender_id: string;
};

// The `drawing://sync` event payload: the room's authoritative drawing
// state as of drawing sequence `seq`, sent when this client's room
// subscription dropped events. It replaces the drawing state; only
// drawing events with a higher `seq` follow.
export type StrokeSyncPoint = {
    x: number;
    y: number;
    pressure: number;
    ts_ms: number;
};

export type StrokeSyncBegin = {
    tool: string;
    color: string;
    width: number;
    x: number;
    y: number;
    pressure: number;
    ts_ms: number;
};

export type StrokeSyncStrokeEvent = {
    stroke_id: string;
    owner_id: string;
    /** `null` when the server no longer holds this stroke's content:
     *  keep the copy already on screen, if any. */
    begin: StrokeSyncBegin | null;
    points: StrokeSyncPoint[];
    /** Set once the stroke ended; `null` while it is being drawn. */
    end_ts_ms: number | null;
};

export type StrokeSyncEvent = {
    room_id: string;
    seq: number;
    strokes: StrokeSyncStrokeEvent[];
};
export type DownloadSessionIpc = {
  download_id: string;
  media_id: string;
  state: string;
  dedup_hit: boolean;
  total_bytes: number;
  transferred_bytes: number;
  on_disk_path: string | null;
  transfer_started: boolean;
};

export type SharedMediaIpc = {
  id: string;
  filename: string;
  size_bytes: number;
  mime: string;
  sha256: string;
};

export type SharedManifestIpc = {
  room_id: string;
  version: number | null;
  media: SharedMediaIpc[];
};

// The signed manifest itself is opaque to the webview; only the
// Rust side verifies and reads it.
export type ManifestResponsePayload = {
  manifest: unknown;
  version: number;
  published_at_ms: number;
};

export type ManifestStateEvent = {
  room_id: string;
  manifest_hash: string;
  version: number;
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

// P6-T03: the `chat://message` event payload. Emitted
// when a remote CHAT_MESSAGE is accepted and rebroadcast
// by the server. The sender_id is the server-authoritative
// originator (from the validated bearer).
export type ChatMessage = {
    room_id: string;
    sender_id: string;
    sender_name: string;
    text: string;
    reply_to: string | null;
    ts_ms: number;
};

// P6-T06: temp file info returned by getTempFiles.
export type TempFileInfo = {
    file_id: string;
    room_id: string;
    filename: string;
    size_bytes: number;
    created_ms: number;
    owner_user_id: string;
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
  // Emitted after a manifest passes signature + trust-anchor checks.
  manifestState: <EventListener<ManifestStateEvent>>((h) =>
    __listenAs__("manifest://state", h)
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
  // P5-T03: DRAW_UNDO accepted by the server (actor included).
  // Removes the named stroke from the canvas.
  strokeUndo: <EventListener<StrokeUndoEvent>>((h) =>
    __listenAs__("drawing://undo", h)
  ),
  // P5-T03: DRAW_CLEAR accepted by the server (actor included).
  // Wipes every stroke.
  strokeClear: <EventListener<StrokeClearEvent>>((h) =>
    __listenAs__("drawing://clear", h)
  ),
  // P5-T04: another participant's laser position (relayed
  // LASER_MOVE, never the local user's own).
  laserMove: <EventListener<LaserMoveEvent>>((h) =>
    __listenAs__("laser://move", h)
  ),
  // P5-T04: another participant released its laser (relayed
  // LASER_OFF). Fades that sender's trail.
  laserOff: <EventListener<LaserOffEvent>>((h) =>
    __listenAs__("laser://off", h)
  ),
  // P6-T03: inbound CHAT_MESSAGE from a remote participant.
  // Emitted when a chat message is accepted and rebroadcast
  // by the server. The sender_id is the server-authoritative
  // originator.
  chatMessage: <EventListener<ChatMessage>>((h) =>
    __listenAs__("chat://message", h)
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
export const strokeUndoChanged = events.strokeUndo;
export const strokeClearChanged = events.strokeClear;
export const laserMoveChanged = events.laserMove;
export const laserOffChanged = events.laserOff;
export const chatMessageChanged = events.chatMessage;
