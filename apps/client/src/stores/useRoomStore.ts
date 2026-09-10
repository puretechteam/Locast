import { create } from "zustand";
import type { ConnectionState, RoomSummaryIpc } from "../services/room";

export type { RoomSummaryIpc, ParticipantIpc, ParticipantStatusIpc } from "../services/room";

interface RoomState {
    summary: RoomSummaryIpc | null;
    signaling: ConnectionState | null;
    lastKnownHostPositionMs: number | null;
    setSummary: (summary: RoomSummaryIpc | null) => void;
    setSignaling: (signaling: ConnectionState) => void;
    setLastKnownHostPositionMs: (positionMs: number | null) => void;
    clear: () => void;
}

export const useRoomStore = create<RoomState>((set) => ({
    summary: null,
    signaling: null,
    lastKnownHostPositionMs: null,
    setSummary: (summary) => set({ summary }),
    setSignaling: (signaling) => set({ signaling }),
    setLastKnownHostPositionMs: (lastKnownHostPositionMs) => set({ lastKnownHostPositionMs }),
    clear: () => set({ summary: null, signaling: null, lastKnownHostPositionMs: null }),
}));
