// apps/client/src/services/mediaUrl.ts
//
// Turns the opaque `locast://media/...` URL returned by the `media_resolve_url`
// command into the URL this platform's webview can actually load (P1-T10).
//
// WebView2 (Windows) and Android cannot navigate to a custom scheme; Tauri
// serves it at `http://locast.localhost/`. macOS and Linux use
// `locast://localhost/`. Tauri's own `convertFileSrc` knows which one applies,
// so the base comes from it and the rest of the URL passes through unchanged.
// The Rust handler accepts every one of these forms.

import { convertFileSrc } from "@tauri-apps/api/core";

const SCHEME_PREFIX = "locast://";

/** The platform's base URL for the `locast` scheme, ending in `/`. */
export function locastWebviewBase(): string {
    try {
        return convertFileSrc("", "locast");
    } catch {
        // No Tauri runtime (a plain browser): fall back to the form used on
        // macOS and Linux.
        return `${SCHEME_PREFIX}localhost/`;
    }
}

/** Map `locast://media/<sha>/<name>` onto `base` + `media/<sha>/<name>`. */
export function toWebviewUrl(resolved: string, base: string = locastWebviewBase()): string {
    if (!resolved.startsWith(SCHEME_PREFIX)) {
        throw new Error(`not a locast:// URL: ${resolved}`);
    }
    const rest = resolved.slice(SCHEME_PREFIX.length).replace(/^\/+/, "");
    const root = base.endsWith("/") ? base : `${base}/`;
    return `${root}${rest}`;
}
