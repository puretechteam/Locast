// apps/client/src/services/settings.ts
//
// Typed wrapper over the Settings IPC surface: the server address the
// signaling client connects to.

import { commands } from "./ipc";
import type { ServerSettingsIpc } from "../bindings";

export type { ServerSettingsIpc } from "../bindings";

/** The saved server address and the one the running client is using. */
export async function getServerSettings(): Promise<ServerSettingsIpc> {
    return await commands.settingsGetServer();
}

/**
 * Save a server address, or clear it with `null`. Rust validates it and
 * rejects anything but `wss://` (or `ws://` for localhost). A saved
 * address applies the next time the app starts.
 */
export async function setServerUrl(url: string | null): Promise<ServerSettingsIpc> {
    return await commands.settingsSetServerUrl(url);
}
