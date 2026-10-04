// P5-T04: laser pointer network transport (React wiring).
//
// Couples the local laser overlay (`LaserPointer`) to the room:
//
// - Outbound: `move(x, y)` / `off()` feed a `LaserSender`
//   (src/laser/laserTransport.ts), which sends LASER_MOVE /
//   LASER_OFF through the `laser_send` Tauri command at most
//   60 Hz, with a ~1 Hz keepalive while the laser is held still.
//   Nothing is sent unless `canSend` (in a room and holding
//   `cap.LASER`; the host holds every cap). The server enforces
//   the cap anyway and drops denials silently.
// - Inbound: subscribes (through `listenEvent`, so the
//   Playwright shim can drive it) to `laser://move` and
//   `laser://off`, filters each event with `acceptRemoteLaser`
//   against the current room and the local user's
//   server-assigned id, and hands it to `onRemoteMove` /
//   `onRemoteOff`.
// - Lifecycle: one sender and one subscription per room. On a
//   room change or unmount the listeners are removed, the sender
//   is closed (a held laser gets its final LASER_OFF) and
//   `onReset` clears the remote trails.

import { useCallback, useEffect, useMemo, useRef } from "react";
import { listenEvent } from "../services/_eventTransport";
import { commands } from "../services/ipc";
import { LaserSender, acceptRemoteLaser } from "../laser/laserTransport";
import type { LaserSendFn } from "../laser/laserTransport";
import type { LaserMoveEvent, LaserOffEvent } from "../bindings";

export interface UseLaserTransportArgs {
    /** The room the local user is in; `null` sends and receives
     *  nothing. */
    roomId: string | null;
    /** The local user's server-assigned id (filters out any echo
     *  of the local user's own laser). */
    localUserId: string | null;
    /** Whether the local user may send (holds `cap.LASER`). */
    canSend: boolean;
    /** A remote user's laser moved to (x, y). */
    onRemoteMove: (senderId: string, x: number, y: number) => void;
    /** A remote user released its laser. */
    onRemoteOff: (senderId: string) => void;
    /** Drop every remote trail (room change / unmount). */
    onReset: () => void;
    /** Transport override (default: `commands.laserSend`). */
    send?: LaserSendFn;
}

export interface LaserTransportHandle {
    /** The local laser is at (x, y), normalized and clamped. */
    move: (x: number, y: number) => void;
    /** The local user released the laser. */
    off: () => void;
}

export function useLaserTransport(args: UseLaserTransportArgs): LaserTransportHandle {
    const { roomId } = args;

    // Latest values for callbacks that outlive a render.
    const argsRef = useRef(args);
    argsRef.current = args;

    const senderRef = useRef<LaserSender | null>(null);

    useEffect(() => {
        if (roomId === null) return;
        let cancelled = false;
        const unsubs: Array<() => void> = [];

        const send: LaserSendFn =
            argsRef.current.send ?? ((input) => commands.laserSend(input));
        const sender = new LaserSender(send);
        senderRef.current = sender;

        const ctx = () => ({
            roomId,
            localUserId: argsRef.current.localUserId,
        });

        const onMove = (ev: LaserMoveEvent): void => {
            if (cancelled) return;
            if (!acceptRemoteLaser(ev, ctx(), "move")) return;
            argsRef.current.onRemoteMove(ev.sender_id, ev.x, ev.y);
        };
        const onOff = (ev: LaserOffEvent): void => {
            if (cancelled) return;
            if (!acceptRemoteLaser(ev, ctx(), "off")) return;
            argsRef.current.onRemoteOff(ev.sender_id);
        };

        (async () => {
            try {
                const u1 = await listenEvent<LaserMoveEvent>("laser://move", onMove);
                if (cancelled) { u1(); return; }
                unsubs.push(u1);
                const u2 = await listenEvent<LaserOffEvent>("laser://off", onOff);
                if (cancelled) { u2(); return; }
                unsubs.push(u2);
                if (import.meta.env.MODE === "test") {
                    (window as unknown as { __locast_laser_subscribed?: boolean })
                        .__locast_laser_subscribed = true;
                }
            } catch (err) {
                console.warn("useLaserTransport: listen failed", err);
            }
        })();

        return () => {
            cancelled = true;
            for (const u of unsubs) {
                try { u(); } catch { /* swallow */ }
            }
            if (import.meta.env.MODE === "test") {
                (window as unknown as { __locast_laser_subscribed?: boolean })
                    .__locast_laser_subscribed = false;
            }
            // Releases the laser remotely if it was showing (an
            // unmount while held would otherwise leave a stuck
            // trail until the receivers' idle expiry).
            sender.close();
            if (senderRef.current === sender) senderRef.current = null;
            argsRef.current.onReset();
        };
    }, [roomId]);

    // Losing the capability while the laser is held releases it
    // remotely (best effort: the server may already refuse it).
    useEffect(() => {
        if (!args.canSend) senderRef.current?.off();
    }, [args.canSend]);

    const move = useCallback((x: number, y: number) => {
        if (!argsRef.current.canSend) return;
        senderRef.current?.move(x, y);
    }, []);

    const off = useCallback(() => {
        senderRef.current?.off();
    }, []);

    return useMemo(() => ({ move, off }), [move, off]);
}
