import { create } from "zustand";
import type { ConnectionState, RoomSummaryIpc } from "../services/room";

export type { RoomSummaryIpc, ParticipantIpc, ParticipantStatusIpc } from "../services/room";

interface RoomState {
    summary: RoomSummaryIpc | null;
    signaling: ConnectionState | null;
    setSummary: (summary: RoomSummaryIpc | null) => void;
    setSignaling: (signaling: ConnectionState) => void;
    clear: () => void;
}

export const useRoomStore = create<RoomState>((set) => ({
    summary: null,
    signaling: null,
    setSummary: (summary) => set({ summary }),
    setSignaling: (signaling) => set({ signaling }),
    clear: () => set({ summary: null }),
}));
