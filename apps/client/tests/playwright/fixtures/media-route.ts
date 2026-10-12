// Serves a real media file to the page with HTTP Range support, the way the
// desktop app's `locast://` handler does, so a Chromium <video> can load it,
// report its real duration, and seek by byte range.
//
// Only the media bytes are real here. The Tauri IPC, the room and the signaling
// server are still the harness's shim (see playback_e2e.spec.ts).

import { createReadStream, statSync } from "node:fs";
import type { Page } from "@playwright/test";

export interface MediaRouteStats {
    /** Every request the page made for the file: its Range header and the
     *  status served. */
    requests: Array<{ range: string | null; status: number; bytes: number }>;
}

/** Largest body served per response. A real <video> asks open-ended ranges
 *  (`bytes=N-`) and happily takes a shorter 206, which keeps the harness from
 *  buffering a whole multi-hundred-MB file per request. */
const MAX_BODY = 4 * 1024 * 1024;

async function readSlice(path: string, start: number, end: number): Promise<Buffer> {
    const chunks: Buffer[] = [];
    for await (const c of createReadStream(path, { start, end })) {
        chunks.push(c as Buffer);
    }
    return Buffer.concat(chunks);
}

/** Route `urlGlob` (for example "**\/test/clip.webm") to the file at `path`. */
export async function serveMedia(
    page: Page,
    urlGlob: string,
    path: string,
    contentType: string,
): Promise<MediaRouteStats> {
    const size = statSync(path).size;
    const stats: MediaRouteStats = { requests: [] };
    await page.route(urlGlob, async (route) => {
        const header = route.request().headers()["range"] ?? null;
        const base = {
            "Content-Type": contentType,
            "Accept-Ranges": "bytes",
            "Cache-Control": "no-store",
        };
        if (header === null) {
            const end = Math.min(size, MAX_BODY) - 1;
            const body = await readSlice(path, 0, end);
            const partial = size > MAX_BODY;
            stats.requests.push({ range: null, status: partial ? 206 : 200, bytes: body.length });
            await route.fulfill({
                status: partial ? 206 : 200,
                headers: {
                    ...base,
                    "Content-Length": String(body.length),
                    ...(partial ? { "Content-Range": `bytes 0-${end}/${size}` } : {}),
                },
                body,
            });
            return;
        }
        const m = /^bytes=(\d*)-(\d*)$/.exec(header.trim());
        let start: number;
        let end = size - 1;
        if (m === null || (m[1] === "" && m[2] === "")) {
            stats.requests.push({ range: header, status: 416, bytes: 0 });
            await route.fulfill({
                status: 416,
                headers: { ...base, "Content-Range": `bytes */${size}` },
            });
            return;
        }
        if (m[1] === "") {
            start = Math.max(0, size - Number(m[2]));
        } else {
            start = Number(m[1]);
            if (m[2] !== "") end = Math.min(size - 1, Number(m[2]));
        }
        if (start >= size || start > end) {
            stats.requests.push({ range: header, status: 416, bytes: 0 });
            await route.fulfill({
                status: 416,
                headers: { ...base, "Content-Range": `bytes */${size}` },
            });
            return;
        }
        end = Math.min(end, start + MAX_BODY - 1);
        const body = await readSlice(path, start, end);
        stats.requests.push({ range: header, status: 206, bytes: body.length });
        await route.fulfill({
            status: 206,
            headers: {
                ...base,
                "Content-Length": String(body.length),
                "Content-Range": `bytes ${start}-${end}/${size}`,
            },
            body,
        });
    });
    return stats;
}
