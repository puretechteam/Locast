// Unit test for the chat history store: ordering and the size cap.
//
// Run by `pnpm test` (scripts/run-unit-tests.mjs), which picks up every
// `*.smoke.ts` under src/.

import { MAX_CHAT_MESSAGES, useChatStore } from "./useChatStore.ts";

let failures = 0;

function check(name: string, cond: boolean): void {
    if (cond) {
        process.stdout.write(`  ok ${name}\n`);
    } else {
        process.stdout.write(`  FAIL ${name}\n`);
        failures++;
    }
}

function msg(n: number) {
    return {
        room_id: "room-1",
        sender_id: "sender-1",
        sender_name: "someone",
        text: `message ${n}`,
        reply_to: null,
        ts_ms: 1_700_000_000_000 + n,
    };
}

useChatStore.getState().clear();
check("the history starts empty", useChatStore.getState().messages.length === 0);

useChatStore.getState().add(msg(1));
useChatStore.getState().add(msg(2));
useChatStore.getState().add(msg(3));
check(
    "messages are kept in arrival order",
    useChatStore.getState().messages.map((m) => m.text).join("|") === "message 1|message 2|message 3",
);

useChatStore.getState().clear();
check("clear forgets the history", useChatStore.getState().messages.length === 0);

for (let i = 0; i < MAX_CHAT_MESSAGES + 25; i++) {
    useChatStore.getState().add(msg(i));
}
const kept = useChatStore.getState().messages;
check("the history is capped", kept.length === MAX_CHAT_MESSAGES);
check("the newest message is kept", kept[kept.length - 1]?.text === `message ${MAX_CHAT_MESSAGES + 24}`);
check("the oldest messages are the ones dropped", kept[0]?.text === "message 25");

useChatStore.getState().clear();
useChatStore.getState().add(msg(1));
const before = useChatStore.getState().messages;
useChatStore.getState().add(msg(2));
check(
    "adding does not mutate the previous array (so subscribers re-render)",
    before.length === 1 && useChatStore.getState().messages.length === 2,
);

if (failures > 0) {
    process.stdout.write(`\n${failures} check(s) failed\n`);
    process.exit(1);
}
process.stdout.write("\nAll checks passed.\n");
