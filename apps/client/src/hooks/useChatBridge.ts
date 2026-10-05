import { useEffect } from "react";
import { events } from "../services/ipc";
import { useChatStore } from "../stores/useChatStore";

/**
 * Feeds `chat://message` events into the chat store for as long as the app is
 * open. The room page used to own this listener, so it was gone whenever the
 * page was unmounted (while a download runs the blocking guard hides it), and
 * every message sent meanwhile was lost. Mounted at the app level next to
 * `SharedMediaBridge`. Renders nothing.
 */
export function ChatBridge(): null {
    useEffect(() => {
        let cancelled = false;
        let unlisten: (() => void) | undefined;
        events
            .chatMessage((message) => {
                if (!cancelled) useChatStore.getState().add(message);
            })
            .then((u) => {
                if (cancelled) u();
                else unlisten = u;
            })
            .catch((err: unknown) => {
                console.warn("ChatBridge: could not subscribe", err);
            });
        return () => {
            cancelled = true;
            unlisten?.();
        };
    }, []);
    return null;
}
