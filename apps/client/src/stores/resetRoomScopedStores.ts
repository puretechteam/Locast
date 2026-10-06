// apps/client/src/stores/resetRoomScopedStores.ts
//
// Forget everything that belongs to the room the user was in. Call it
// whenever the user is no longer in a room: after leaving (from the footer or
// from the download dialog) and when the room ends or the user is removed.
//
// Each of these stores held its value after a room ended, so the next visit to
// a room page showed the dead room, "Playing locally: <old room's file>" and a
// leftover capability set or connection-quality indicator, and joining another
// room started with the previous room's video and positions.

import { useCapabilityStore } from "./useCapabilityStore";
import { useChatStore } from "./useChatStore";
import { useClockSkewStore } from "./useClockSkewStore";
import { useConnectionQualityStore } from "./useConnectionQualityStore";
import { useDownloadStore } from "./useDownloadStore";
import { useDrawingStore } from "./useDrawingStore";
import { usePlaybackStore } from "./usePlaybackStore";
import { useRoomStore } from "./useRoomStore";
import { useSharedMediaStore } from "./useSharedMediaStore";
import { useViewerPositionStore } from "./useViewerPositionStore";

export function resetRoomScopedStores(): void {
    // Also clears the playback store's media source, ready flag, parked event
    // and sequence counter, so a re-join starts from a clean slate.
    usePlaybackStore.getState().clear();
    useViewerPositionStore.getState().clear();
    useRoomStore.getState().clear();
    useCapabilityStore.getState().clear();
    useChatStore.getState().clear();
    useConnectionQualityStore.getState().clear();
    useClockSkewStore.getState().clear();
    // Strokes, the active-stroke map and the sequence counter belong to the
    // room. Without this a re-join of the same room id kept the old strokes
    // and `lastSeq`, because `setRoomId` is a no-op for an unchanged id.
    useDrawingStore.getState().setRoomId(null);
    // Stops the shared-media bridge's retry loop for the old room.
    useSharedMediaStore.getState().reset(null, false);
    // Leaving cancels this room's transfers, so their rows no longer describe
    // anything; a ended room's pending row would otherwise keep the blocking
    // dialog up.
    useDownloadStore.getState().clear();
}
