// apps/client/src/services/downloads.ts
//
// Typed wrapper over the Rust download-event IPC surface.
// P3-T08: typed listeners for the `download://state` and
// `download://progress` Tauri events emitted by the
// receiver-side transfer session.

import { events } from "./ipc";
import type { DownloadStateEvent, DownloadProgressEvent } from "../bindings";

export type { DownloadStateEvent, DownloadProgressEvent };

/** Subscribe to `download://state`. The handler receives the
 *  immediate state-transition event. Returns an unsubscribe
 *  function. */
export async function onDownloadState(
    handler: (e: DownloadStateEvent) => void,
): Promise<() => void> {
    return await events.downloadState(handler);
}

/** Subscribe to `download://progress`. The handler receives
 *  the coalesced progress event (at most 5 Hz per download).
 *  Returns an unsubscribe function. */
export async function onDownloadProgress(
    handler: (e: DownloadProgressEvent) => void,
): Promise<() => void> {
    return await events.downloadProgress(handler);
}