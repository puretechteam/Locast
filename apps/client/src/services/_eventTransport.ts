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
    // In test mode, the test harness (vite-app.ts) imports the tauriShim
    // module and uses it to emit events. The same module instance must be
    // shared here so that events emitted by the harness are received by
    // the listeners registered by this transport.
    // The module is cached on window.__tauriShim by the test harness.
    const mod = await import("/tests/playwright/shim/tauriShim" as string);
    shim = mod.listen as unknown as ListenFn;
    // Also expose on window for sharing with the harness if not already there
    const w = typeof window !== "undefined" ? window as unknown as { __tauriShim?: typeof mod } : null;
    if (w && !w.__tauriShim) {
        w.__tauriShim = mod;
    }
    return shim;
}

export async function listenEvent<T>(
    event: string,
    handler: (payload: T) => void,
): Promise<() => void> {
    const l = await getListener();
    return l(event, (e) => handler(e.payload as T));
}
