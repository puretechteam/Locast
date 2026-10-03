// apps/client/src/services/mediaCatalog.ts
//
// Typed wrapper over the library IPC surface (P1-T09): list/search the
// catalog, promote an item to permanent, delete it, and import files. The
// stores and components talk to this module only; it is the one place that
// knows about the native file picker.

import { invoke } from "@tauri-apps/api/core";
import { commands } from "./ipc";
import type { ImportedMedia, LibraryItem } from "../bindings";

export type { ImportedMedia, LibraryItem };

/** Extensions offered first in the picker. "All files" stays available. */
const MEDIA_EXTENSIONS = ["mp4", "m4v", "mkv", "webm", "mov", "avi", "mp3", "m4a", "flac", "ogg", "wav"];

/** List the library, newest first. A non-blank `query` searches by file name. */
export async function listLibrary(query: string): Promise<LibraryItem[]> {
    const trimmed = query.trim();
    return await commands.libraryList(trimmed === "" ? null : trimmed, null, null);
}

/** Promote a temporary item to permanent. */
export async function makePermanent(id: string): Promise<void> {
    await commands.libraryMakePermanent(id);
}

/** Delete an item from the library (the file goes to the library trash). */
export async function deleteFromLibrary(id: string): Promise<void> {
    await commands.libraryDelete(id);
}

/** Import files by absolute path. */
export async function importMedia(paths: string[]): Promise<ImportedMedia[]> {
    return await commands.mediaImport(paths);
}

/**
 * Open the native file picker (the Tauri dialog plugin, permission
 * `dialog:allow-open`) and return the chosen absolute paths, or an empty
 * array if the user cancelled.
 */
export async function pickMediaFiles(): Promise<string[]> {
    const picked = await invoke<string[] | string | null>("plugin:dialog|open", {
        options: {
            title: "Import media files",
            multiple: true,
            directory: false,
            filters: [
                { name: "Media files", extensions: MEDIA_EXTENSIONS },
                { name: "All files", extensions: ["*"] },
            ],
        },
    });
    if (picked === null || picked === undefined) return [];
    return Array.isArray(picked) ? picked : [picked];
}
