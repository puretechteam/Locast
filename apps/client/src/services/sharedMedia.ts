// Host media sharing and viewer acquisition (slice 3). Thin wrappers
// over the manifest / invite / download commands; every security
// decision (host authorization, signature, trust anchor, dedup, chunk
// and file verification) stays in Rust and on the server.

import { commands } from "./ipc";
import { listenEvent } from "./_eventTransport";
import type {
    DownloadSessionIpc,
    ManifestStateEvent,
    SharedManifestIpc,
    SharedMediaIpc,
} from "../bindings";

export type { DownloadSessionIpc, ManifestStateEvent, SharedManifestIpc, SharedMediaIpc };

/** The server ignores the fetch's media id and returns the latest
 * manifest; a nil UUID keeps the argument well-formed. */
const ANY_MEDIA_ID = "00000000-0000-0000-0000-000000000000";

/** Host only: the invite link viewers join with. */
export async function getInviteUrl(): Promise<string> {
    return await commands.roomInviteUrl();
}

/** Host only (server-enforced): share exactly these library items. */
export async function shareMedia(mediaIds: string[]): Promise<void> {
    await commands.manifestPublish(mediaIds);
}

/** The verified manifest cached for the current room, if any. */
export async function currentManifest(): Promise<SharedManifestIpc | null> {
    return await commands.manifestCurrent();
}

/** Late-join: ask the server for the current manifest. Rust accepts it
 * only after the signature and trust-anchor checks. */
export async function fetchManifest(): Promise<void> {
    await commands.manifestFetch(ANY_MEDIA_ID);
}

/** Start (or resume, or dedup-resolve) the download of one shared item. */
export async function openDownload(mediaId: string): Promise<DownloadSessionIpc> {
    return await commands.downloadOpen(mediaId);
}

export async function onManifestState(
    handler: (e: ManifestStateEvent) => void,
): Promise<() => void> {
    return await listenEvent<ManifestStateEvent>("manifest://state", handler);
}
