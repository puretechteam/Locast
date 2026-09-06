import { useCallback, useRef, useEffect, useState } from "react";
import { useCapabilityStore, CAP } from "../stores/useCapabilityStore";
import { sendChatMessage } from "../services/chat";
import type { ChatMessage } from "../services/chat";

interface ChatPanelProps {
    messages: ChatMessage[];
}

const MAX_TEXT_LENGTH = 2048;

export function ChatPanel({ messages }: ChatPanelProps): React.ReactNode {
    const hasChat = useCapabilityStore(
        (s) => s.youCapSet !== null && (s.youCapSet & CAP.CHAT) !== 0,
    );
    const [text, setText] = useState("");
    const [replyTo, setReplyTo] = useState<string | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [sending, setSending] = useState(false);
    const bottomRef = useRef<HTMLDivElement>(null);
    const inputRef = useRef<HTMLTextAreaElement>(null);

    useEffect(() => {
        bottomRef.current?.scrollIntoView({ behavior: "smooth" });
    }, [messages]);

    const handleTextChange = useCallback(
        (e: React.ChangeEvent<HTMLTextAreaElement>) => {
            const value = e.target.value;
            setText(value);
            if (value.length > MAX_TEXT_LENGTH) {
                setError(`Message exceeds ${MAX_TEXT_LENGTH} character limit (2 KiB)`);
            } else {
                setError(null);
            }
        },
        [],
    );

    const handleSend = useCallback(async () => {
        const trimmed = text.trim();
        if (!trimmed || trimmed.length > MAX_TEXT_LENGTH || sending) return;

        setSending(true);
        setError(null);
        try {
            await sendChatMessage(trimmed, replyTo ?? undefined);
            setText("");
            setReplyTo(null);
        } catch (err) {
            const msg = err instanceof Error ? err.message : String(err);
            setError(`Send failed: ${msg}`);
        } finally {
            setSending(false);
            inputRef.current?.focus();
        }
    }, [text, replyTo, sending]);

    const handleKeyDown = useCallback(
        (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
            if (e.key === "Enter" && !e.shiftKey) {
                e.preventDefault();
                void handleSend();
            }
        },
        [handleSend],
    );

    const handleReplyClick = useCallback((msgId: string) => {
        setReplyTo(msgId);
        inputRef.current?.focus();
    }, []);

    const handleCancelReply = useCallback(() => {
        setReplyTo(null);
    }, []);

    const formatTime = (tsMs: number): string => {
        const d = new Date(tsMs);
        return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
    };

    if (!hasChat) return null;

    return (
        <section
            className="chat-panel"
            aria-label="Chat"
            data-testid="chat-panel"
        >
            <div className="chat-panel__messages" role="log" aria-live="polite">
                {messages.length === 0 && (
                    <p className="chat-panel__empty" data-testid="chat-empty">
                        No messages yet
                    </p>
                )}
                {messages.map((msg) => {
                    const isReply = msg.reply_to !== null;
                    return (
                        <div
                            key={`${msg.room_id}-${msg.ts_ms}-${msg.sender_id}`}
                            className="chat-panel__message"
                            data-testid="chat-message"
                            data-sender-id={msg.sender_id}
                        >
                            <span className="chat-panel__message-header">
                                <span
                                    className="chat-panel__message-sender"
                                    data-testid="chat-message-sender"
                                >
                                    {msg.sender_name}
                                </span>
                                <time
                                    className="chat-panel__message-time"
                                    dateTime={new Date(msg.ts_ms).toISOString()}
                                    data-testid="chat-message-time"
                                >
                                    {formatTime(msg.ts_ms)}
                                </time>
                            </span>
                            {isReply && (
                                <span
                                    className="chat-panel__message-reply-indicator"
                                    data-testid="chat-message-reply"
                                    title={`Replying to ${msg.reply_to}`}
                                >
                                    ↳
                                </span>
                            )}
                            <p className="chat-panel__message-text" data-testid="chat-message-text">
                                {msg.text}
                            </p>
                            <button
                                className="chat-panel__message-reply-btn"
                                onClick={() =>
                                    handleReplyClick(`${msg.room_id}-${msg.ts_ms}-${msg.sender_id}`)
                                }
                                title="Reply"
                                aria-label={`Reply to ${msg.sender_name}`}
                                data-testid="chat-message-reply-btn"
                            >
                                Reply
                            </button>
                        </div>
                    );
                })}
                <div ref={bottomRef} />
            </div>

            {replyTo !== null && (
                <div className="chat-panel__reply-banner" data-testid="chat-reply-banner">
                    <span>Replying to a message</span>
                    <button
                        className="chat-panel__reply-cancel"
                        onClick={handleCancelReply}
                        aria-label="Cancel reply"
                        data-testid="chat-reply-cancel"
                    >
                        Cancel
                    </button>
                </div>
            )}

            <div className="chat-panel__compose">
                <textarea
                    ref={inputRef}
                    className="chat-panel__input"
                    value={text}
                    onChange={handleTextChange}
                    onKeyDown={handleKeyDown}
                    placeholder="Type a message..."
                    rows={2}
                    maxLength={MAX_TEXT_LENGTH + 100}
                    aria-label="Message text"
                    aria-invalid={error !== null}
                    aria-describedby={error ? "chat-panel-error" : undefined}
                    data-testid="chat-input"
                />
                {error && (
                    <p
                        className="chat-panel__error"
                        id="chat-panel-error"
                        role="alert"
                        data-testid="chat-error"
                    >
                        {error}
                    </p>
                )}
                <button
                    className="chat-panel__send"
                    onClick={() => void handleSend()}
                    disabled={
                        !text.trim() ||
                        text.trim().length > MAX_TEXT_LENGTH ||
                        sending
                    }
                    aria-label="Send message"
                    data-testid="chat-send-btn"
                >
                    {sending ? "Sending..." : "Send"}
                </button>
            </div>
        </section>
    );
}
