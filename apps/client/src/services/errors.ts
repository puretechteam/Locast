// apps/client/src/services/errors.ts
//
// A readable message for whatever a rejected IPC call produced. Rust's
// `AppError` reaches the webview as a plain object tagged with `kind` (and,
// for most variants, a `message`), not as an `Error`, so `String(err)` would
// show "[object Object]".

export function errorText(err: unknown): string {
    if (err instanceof Error) return err.message;
    if (typeof err === "string") return err;
    if (typeof err === "object" && err !== null) {
        const { message, kind } = err as { message?: unknown; kind?: unknown };
        if (typeof message === "string" && message.length > 0) return message;
        if (typeof kind === "string" && kind.length > 0) return kind;
    }
    return String(err);
}
