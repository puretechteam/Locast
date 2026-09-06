import { commands } from "./ipc";

export interface ChatMessage {
    room_id: string;
    sender_id: string;
    sender_name: string;
    text: string;
    reply_to: string | null;
    ts_ms: number;
}

export async function sendChatMessage(text: string, replyTo?: string): Promise<void> {
    await commands.roomChatMessage(text, replyTo ?? null);
}
