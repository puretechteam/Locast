// Display helpers for library tiles. Pure functions, no I/O.

const UNITS = ["B", "KB", "MB", "GB", "TB"];

/** 1536 -> "1.5 KB"; one decimal below 10, none above. */
export function formatBytes(bytes: number): string {
    if (!Number.isFinite(bytes) || bytes < 0) return "";
    let value = bytes;
    let unit = 0;
    while (value >= 1024 && unit < UNITS.length - 1) {
        value /= 1024;
        unit++;
    }
    const text = unit === 0 || value >= 10 ? value.toFixed(0) : value.toFixed(1);
    return `${text} ${UNITS[unit]}`;
}

/** 5025000 -> "1:23:45"; 65000 -> "1:05". */
export function formatDuration(ms: number): string {
    const total = Math.max(0, Math.round(ms / 1000));
    const h = Math.floor(total / 3600);
    const m = Math.floor((total % 3600) / 60);
    const s = total % 60;
    const ss = String(s).padStart(2, "0");
    return h > 0 ? `${h}:${String(m).padStart(2, "0")}:${ss}` : `${m}:${ss}`;
}
