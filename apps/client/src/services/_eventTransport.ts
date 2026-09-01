import { listen } from "@tauri-apps/api/event";

const isTest = import.meta.env.MODE === "test";

type ListenFn = (
    event: string,
    handler: (e: { event: string; id: number; payload: unknown }) => void,
) => Promise<() => void>;

let shim: ListenFn | undefined;
async function getListener(): Promise<ListenFn> {
    if (!isTest) return listen as unknown as ListenFn;
    if (shim) return shim;
    // IMPORTANT: Vite must resolve this dynamic import to the same module URL as the test-side /src/tests/.../tauriShim.ts import. Both URLs serve the same file. Do not add cache-busting query strings to either.
    const mod = await import("../../tests/playwright/shim/tauriShim");
    shim = mod.listen as unknown as ListenFn;
    return shim;
}

export async function listenEvent<T>(
    event: string,
    handler: (payload: T) => void,
): Promise<() => void> {
    const l = await getListener();
    return l(event, (e) => handler(e.payload as T));
}
