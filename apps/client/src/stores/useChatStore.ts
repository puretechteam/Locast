import { create } from "zustand";
import type { ChatMessage } from "../bindings";

/** Most messages kept. The history only lives in memory for the current room,
 *  and it was unbounded: a long session in a busy room grew it for ever. */
export const MAX_CHAT_MESSAGES = 500;

interface ChatState {
    messages: ChatMessage[];
    /** Append a message, dropping the oldest ones past [`MAX_CHAT_MESSAGES`]. */
    add: (message: ChatMessage) => void;
    /** Forget the history (the user is no longer in the room). */
    clear: () => void;
}

/**
 * The current room's chat history. It lives in a store, fed by an app-level
 * listener, rather than in the room page's state: the page is unmounted while a
 * download runs (the blocking guard hides it), and everything it held, and every
 * message that arrived meanwhile, was lost.
 */
export const useChatStore = create<ChatState>((set) => ({
    messages: [],
    add: (message) =>
        set((prev) => {
            const next = [...prev.messages, message];
            return {
                messages:
                    next.length > MAX_CHAT_MESSAGES ? next.slice(next.length - MAX_CHAT_MESSAGES) : next,
            };
        }),
    clear: () => set({ messages: [] }),
}));
