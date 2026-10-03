import { useEffect } from "react";
import { listenEvent } from "../services/_eventTransport";
import { getRoomState } from "../services/room";
import type { RoomSummaryIpc } from "../services/room";
import {
    currentManifest,
    fetchManifest,
    onManifestState,
    openDownload,
} from "../services/sharedMedia";
import { useDownloadStore } from "../stores/useDownloadStore";
import { useSharedMediaStore } from "../stores/useSharedMediaStore";

/** Retry delays while no source peer is connected yet (ms). */
const RETRY_BASE_MS = 1000;
const RETRY_MAX_MS = 5000;

/** Rust's manifest rejections (`ManifestAcceptError`) worth showing. */
const VERIFICATION_FAILURE = /trust anchor|does not match invite|signature/i;

function messageOf(err: unknown): string {
    if (err instanceof Error) return err.message;
    if (typeof err === "string") return err;
    if (typeof err === "object" && err !== null) {
        const o = err as { message?: unknown };
        if (typeof o.message === "string") return o.message;
    }
    return String(err);
}

/**
 * Slice 3 orchestration: follows the room and its verified manifest and,
 * on a viewer, makes every shared item local through `download_open`
 * (dedup first, then the P2P transfer). Mounted next to the
 * `DownloadEventBridge`, OUTSIDE the `DownloadBlockingGuard`, because the
 * guard unmounts the room page while a download is active and the retry
 * loop must keep running. Renders nothing.
 */
export function SharedMediaBridge(): null {
    useEffect(() => {
        let cancelled = false;
        const unsubs: Array<() => void> = [];
        const timers = new Set<ReturnType<typeof setTimeout>>();
        const inflight = new Set<string>();
        let fetchedFor: string | null = null;
        const store = useSharedMediaStore.getState;

        const clearTimers = () => {
            for (const t of timers) clearTimeout(t);
            timers.clear();
        };

        const acquire = async (roomId: string, mediaId: string, attempt: number) => {
            const cur = store().items[mediaId];
            if (attempt === 0 && cur !== undefined && cur.kind !== "error") return;
            if (inflight.has(mediaId)) return;
            inflight.add(mediaId);
            if (cur === undefined) store().setItem(mediaId, { kind: "checking" });
            try {
                const r = await openDownload(mediaId);
                if (cancelled || store().roomId !== roomId) return;
                if (r.state === "complete") {
                    store().setItem(mediaId, {
                        kind: "local",
                        localMediaId: r.media_id,
                        dedup: r.dedup_hit,
                    });
                } else if (r.transfer_started) {
                    store().setItem(mediaId, {
                        kind: "downloading",
                        downloadId: r.download_id,
                        localMediaId: r.media_id,
                    });
                } else {
                    // No source DataChannel open yet: the row stays
                    // pending in Rust; ask again on the same row.
                    store().setItem(mediaId, {
                        kind: "waiting",
                        downloadId: r.download_id,
                        localMediaId: r.media_id,
                    });
                    const delay = Math.min(RETRY_BASE_MS * 2 ** attempt, RETRY_MAX_MS);
                    const t = setTimeout(() => {
                        timers.delete(t);
                        void acquire(roomId, mediaId, attempt + 1);
                    }, delay);
                    timers.add(t);
                }
            } catch (err) {
                if (!cancelled && store().roomId === roomId) {
                    store().setItem(mediaId, { kind: "error", message: messageOf(err) });
                }
            } finally {
                inflight.delete(mediaId);
            }
        };

        const refresh = async () => {
            const roomId = store().roomId;
            if (roomId === null) return;
            let m;
            try {
                m = await currentManifest();
            } catch {
                return;
            }
            if (cancelled || store().roomId !== roomId) return;
            store().setManifest(m);
            if (m !== null && !store().isHost) {
                for (const e of m.media) void acquire(roomId, e.id, 0);
            }
        };

        const onSummary = async (summary: RoomSummaryIpc | null) => {
            const roomId = summary?.id ?? null;
            const isHost =
                summary !== null &&
                summary.you_user_id != null &&
                summary.you_user_id === summary.host_user_id;
            const st = store();
            if (st.roomId !== roomId) {
                clearTimers();
                inflight.clear();
                fetchedFor = null;
                st.reset(roomId, isHost);
            } else if (st.isHost !== isHost) {
                st.setIsHost(isHost);
            }
            if (roomId === null) return;
            await refresh();
            if (cancelled || isHost || fetchedFor === roomId) return;
            if (store().manifest !== null) return;
            // Late join: the host may have shared before we arrived.
            fetchedFor = roomId;
            try {
                await fetchManifest();
            } catch (err) {
                // "No manifest yet" is also an error from the server;
                // only a verification failure is worth surfacing.
                const message = messageOf(err);
                if (
                    !cancelled &&
                    store().roomId === roomId &&
                    store().manifest === null &&
                    VERIFICATION_FAILURE.test(message)
                ) {
                    store().setManifestError(message);
                }
            }
            await refresh();
        };

        // A transfer the bridge started finishes through the download
        // events; fold its terminal state back into the item.
        unsubs.push(
            useDownloadStore.subscribe((ds) => {
                const items = store().items;
                for (const [mediaId, item] of Object.entries(items)) {
                    if (item.kind !== "downloading" && item.kind !== "waiting") continue;
                    const ev = ds.states[item.downloadId];
                    if (ev === undefined) continue;
                    if (ev.state === "complete") {
                        store().setItem(mediaId, {
                            kind: "local",
                            localMediaId: item.localMediaId,
                            dedup: false,
                        });
                    } else if (ev.state === "failed" || ev.state === "cancelled") {
                        store().setItem(mediaId, {
                            kind: "error",
                            message: ev.error_message ?? `download ${ev.state}`,
                        });
                    }
                }
            }),
        );

        (async () => {
            try {
                const listeners = [
                    await onManifestState(() => {
                        if (!cancelled) void refresh();
                    }),
                    await listenEvent<RoomSummaryIpc | null>("room://state", (s) => {
                        if (!cancelled) void onSummary(s);
                    }),
                    await listenEvent<RoomSummaryIpc>("room://event", (s) => {
                        if (!cancelled) void onSummary(s);
                    }),
                ];
                for (const u of listeners) {
                    if (cancelled) u();
                    else unsubs.push(u);
                }
                if (cancelled) return;
                const initial = await getRoomState();
                if (!cancelled) await onSummary(initial);
            } catch (err) {
                console.warn("SharedMediaBridge: setup failed", messageOf(err));
            }
        })();

        return () => {
            cancelled = true;
            clearTimers();
            for (const u of unsubs) {
                try {
                    u();
                } catch {
                    /* swallow */
                }
            }
        };
    }, []);
    return null;
}
