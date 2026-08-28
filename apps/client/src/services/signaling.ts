// apps/client/src/services/signaling.ts
//
// Thin typed wrapper over the Rust signaling IPC surface.
// P2-T03 introduced the native WebSocket client; this module
// exposes its three commands to the rest of the React app.
//
// The frontend never sees the bearer token, the AUTH
// signature, the challenge nonce, or the private key. The
// `ConnectionState` shape is the only thing the webview can
// read.

import { commands } from "./ipc";
import type { ConnectionState } from "../bindings";

export type { ConnectionState, ConnPhase, DisconnectReason } from "../bindings";

/** Read the current connection state. */
export async function getSignalingState(): Promise<ConnectionState> {
    return await commands.signalingGetState();
}

/** Start the native connection loop. Idempotent. */
export async function connect(): Promise<void> {
    await commands.signalingConnect();
}

/** Cancel the connection loop and await its exit. */
export async function disconnect(): Promise<void> {
    await commands.signalingDisconnect();
}
