/**
 * P4-T04: DriftIndicator.
 *
 * UI surface for the smoothed local-vs-host drift. The
 * component is HIDDEN by default and only becomes visible
 * when the smoothed offset exceeds the architecture's
 * 2.0 second threshold (architecture §25.3.2 / §25.3.5 /
 * Risk 9 / roadmap P4-T04).
 *
 * Accessibility: the indicator announces its visibility
 * via a `role="status"` + `aria-live="polite"` so screen
 * readers hear "drift: X seconds behind" without a focus
 * change. Color is paired with a numeric magnitude so the
 * indicator is usable without color vision (architecture
 * §25.6).
 *
 * The "Resync" button is intentionally a no-op stub in
 * P4-T04; the actual seek-to-host behavior ships in
 * P4-T05. The button is rendered so the UI surface
 * matches the architecture's §25.3.2 spec and so the
 * P4-T05 task can wire it up without a UI change.
 *
 * No automatic correction: this component NEVER mutates
 * the local media state. It is observation only.
 */

import type { DriftSmootherResult } from "../drift/useDriftSmoother";

export interface DriftIndicatorProps {
    sample: Pick<DriftSmootherResult, "smoothedDriftMs" | "direction" | "indicatorVisible">;
    /** Called when the user clicks "Resync". Stub in
     *  P4-T04; P4-T05 will wire up the real seek. */
    onResync?: () => void;
}

const SECOND = 1000;

function formatSeconds(ms: number): string {
    const abs = Math.abs(ms);
    if (abs < SECOND) {
        return `${ms} ms`;
    }
    const sec = abs / SECOND;
    // One decimal for >= 10s, two for < 10s. The
    // architecture spec does not require a specific
    // format; this gives a human-readable display.
    return sec >= 10 ? `${sec.toFixed(1)}s` : `${sec.toFixed(2)}s`;
}

export function DriftIndicator({ sample, onResync }: DriftIndicatorProps): React.ReactNode {
    if (!sample.indicatorVisible || sample.smoothedDriftMs === null) {
        return null;
    }
    const directionLabel =
        sample.direction === "ahead" ? "ahead" : "behind";
    const className = `drift-indicator drift-indicator--${sample.direction}`;
    return (
        <div
            className={className}
            data-testid="drift-indicator"
            data-direction={sample.direction}
            role="status"
            aria-live="polite"
        >
            <span className="drift-indicator__icon" aria-hidden="true">
                !
            </span>
            <span className="drift-indicator__text">
                Drift: {formatSeconds(sample.smoothedDriftMs)} {directionLabel}
            </span>
            <button
                type="button"
                className="drift-indicator__resync"
                data-testid="drift-indicator-resync"
                onClick={onResync}
            >
                Resync
            </button>
        </div>
    );
}
