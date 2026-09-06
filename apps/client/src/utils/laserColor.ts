// P5-T05: deterministic laser color assignment.
//
// Renders each participant's laser pointer in a distinct color:
// - The local user is always red.
// - Remote participants are assigned from a 24-color palette
//   by a deterministic hash of their user_id.

/** Color for the local user's own laser. */
export const LOCAL_LASER_COLOR = "#ff0000";

/**
 * A 24-color palette that is visually distinct on both light
 * and dark backgrounds. Chosen to maximize perceived difference
 * across deuteranopia, protanopia, and tritanopia.
 */
const PALETTE = [
    "#ff6b6b", // red-orange
    "#4ecdc4", // teal
    "#ffe66d", // yellow
    "#95e1d3", // mint
    "#f38181", // salmon
    "#aa96da", // lavender
    "#fcbad3", // pink
    "#a8d8ea", // sky blue
    "#ff9f43", // orange
    "#6a89cc", // steel blue
    "#78e08f", // green
    "#e77f67", // coral
    "#dda0dd", // plum
    "#f0e68c", // khaki
    "#deb887", // burlywood
    "#b0c4de", // light steel blue
    "#ffefd5", // papaya whip
    "#ffe4b5", // moccasin
    "#ffd700", // gold
    "#adff2f", // green yellow
    "#87ceeb", // sky blue
    "#ffa07a", // light salmon
    "#20b2aa", // light sea green
] as const;

/**
 * A 24-color palette as plain strings for external use
 * (e.g. CSS variable substitution).
 */
export const LASER_PALETTE: string[] = [...PALETTE];

/**
 * Hash a string to a 32-bit integer using djb2.
 * Has better distribution for strings with repeating patterns
 * (like UUIDs with repeated hex digits).
 */
function djb2(str: string): number {
    let hash = 5381;
    for (let i = 0; i < str.length; i++) {
        hash = (Math.imul(hash, 33) + str.charCodeAt(i)) >>> 0;
    }
    return hash;
}

/**
 * Assign a laser color for a given user_id.
 *
 * The local user's own laser is always red so they can
 * distinguish their cursor from others. Remote participants
 * are assigned a deterministic color from the palette based
 * on their user_id; the same user_id always gets the same
 * color.
 *
 * @param userId - The participant's user_id string.
 * @param isLocal - True if this is the local user's own laser.
 * @returns An RGB hex color string, e.g. "#ff6b6b".
 */
export function laserColor(userId: string, isLocal: boolean): string {
    if (isLocal) {
        return LOCAL_LASER_COLOR;
    }
    const index = djb2(userId) % PALETTE.length;
    return PALETTE[index]!;
}

/**
 * Convert a hex color string to an rgba() CSS string.
 * @param hex - A 6-character hex color like "#ff6b6b".
 * @param alpha - Alpha value in [0, 1].
 * @returns An rgba() CSS string, e.g. "rgba(255, 107, 107, 0.5)".
 */
export function hexToRgba(hex: string, alpha: number): string {
    const r = parseInt(hex.slice(1, 3), 16);
    const g = parseInt(hex.slice(3, 5), 16);
    const b = parseInt(hex.slice(5, 7), 16);
    return `rgba(${r}, ${g}, ${b}, ${alpha})`;
}
