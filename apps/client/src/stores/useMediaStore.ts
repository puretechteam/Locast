// apps/client/src/stores/useMediaStore.ts
//
// Central state for the Library page (P1-T09). Components read this store and
// call its actions; none of them talk to the IPC layer directly.
//
// The SQLite database is the source of truth. After every mutation the store
// updates its own copy to match (so the UI reacts immediately) and a search
// change or import re-reads the list from the backend.

import { create } from "zustand";
import {
    deleteFromLibrary,
    importMedia,
    listLibrary,
    makePermanent as makePermanentIpc,
    pickMediaFiles,
} from "../services/mediaCatalog";
import type { LibraryItem } from "../services/mediaCatalog";

export type LoadStatus = "idle" | "loading" | "ready" | "error";

export interface ImportNotice {
    kind: "success" | "error";
    message: string;
}

export interface MediaState {
    items: LibraryItem[];
    query: string;
    status: LoadStatus;
    /** Message for the last failed load. */
    error: string | null;
    importing: boolean;
    /** Outcome of the last import or item action, shown until dismissed. */
    notice: ImportNotice | null;

    /** Re-read the list for the current query. */
    refresh: () => Promise<void>;
    setQuery: (query: string) => Promise<void>;
    /** Open the picker and import what the user chooses. */
    importFiles: () => Promise<void>;
    makePermanent: (id: string) => Promise<void>;
    remove: (id: string) => Promise<void>;
    dismissNotice: () => void;
}

function messageOf(err: unknown): string {
    if (err instanceof Error) return err.message;
    if (typeof err === "string") return err;
    if (typeof err === "object" && err !== null) {
        const o = err as { message?: unknown; kind?: unknown };
        if (typeof o.message === "string") return o.message;
        if (typeof o.kind === "string") return o.kind;
    }
    return String(err);
}

// Every load takes a ticket; only the newest ticket may write results, so a
// slow earlier search can never overwrite a newer one.
let latestLoad = 0;

export const useMediaStore = create<MediaState>((set, get) => ({
    items: [],
    query: "",
    status: "idle",
    error: null,
    importing: false,
    notice: null,

    refresh: async () => {
        const ticket = ++latestLoad;
        // Keep showing the current items while a re-search is in flight;
        // only the very first load shows the loading state.
        if (get().status === "idle" || get().status === "error") {
            set({ status: "loading", error: null });
        }
        try {
            const items = await listLibrary(get().query);
            if (ticket !== latestLoad) return;
            set({ items, status: "ready", error: null });
        } catch (err) {
            if (ticket !== latestLoad) return;
            set({ status: "error", error: messageOf(err) });
        }
    },

    setQuery: async (query) => {
        set({ query });
        await get().refresh();
    },

    importFiles: async () => {
        if (get().importing) return;
        set({ importing: true, notice: null });
        try {
            const paths = await pickMediaFiles();
            if (paths.length === 0) {
                set({ importing: false });
                return;
            }
            const imported = await importMedia(paths);
            set({
                importing: false,
                notice: {
                    kind: "success",
                    message:
                        imported.length === 1
                            ? "Imported 1 file."
                            : `Imported ${imported.length} files.`,
                },
            });
            await get().refresh();
        } catch (err) {
            set({
                importing: false,
                notice: { kind: "error", message: `Import failed: ${messageOf(err)}` },
            });
            await get().refresh();
        }
    },

    makePermanent: async (id) => {
        try {
            await makePermanentIpc(id);
            set({
                items: get().items.map((i) => (i.id === id ? { ...i, status: "permanent" } : i)),
                notice: null,
            });
        } catch (err) {
            set({ notice: { kind: "error", message: `Could not make permanent: ${messageOf(err)}` } });
        }
    },

    remove: async (id) => {
        try {
            await deleteFromLibrary(id);
            set({ items: get().items.filter((i) => i.id !== id), notice: null });
        } catch (err) {
            set({ notice: { kind: "error", message: `Could not delete: ${messageOf(err)}` } });
        }
    },

    dismissNotice: () => set({ notice: null }),
}));

// Test seam, same pattern as the other stores: only in `--mode test`.
if (import.meta.env.MODE === "test") {
    (window as unknown as { __locastMediaStore?: typeof useMediaStore }).__locastMediaStore =
        useMediaStore;
}
