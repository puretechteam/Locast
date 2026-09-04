/**
 * P4-T05: standalone "Sync to Host" button.
 *
 * The component is intentionally tiny: it renders one
 * button that calls the shared `useManualSync` hook
 * (defined in `../drift/useManualSync`). The same hook
 * backs the DriftIndicator's "Resync" button, so the
 * standalone button and the Resync button are
 * guaranteed to behave identically -- there is no
 * parallel sync implementation to drift out of sync.
 *
 * Visibility: the button is rendered when the caller
 * passes a non-null `sync` result (i.e. when the
 * local user is in a room). The button is disabled
 * when `sync.canSync` is false. The disabled reason is
 * surfaced via `aria-disabled` + a `title` attribute
 * for hover tooltips.
 *
 * Branch selection: the hook itself chooses between
 * the local-only branch and the host-authoritative
 * branch based on `isHost`. The button does not need
 * to know which branch will be taken.
 *
 * Test seam: in Vite's test mode the parent's
 * `__locastDrift.syncToHost()` (added in P4-T05) is
 * the same code path the button calls. Playwright
 * tests can drive the seam without rendering the
 * button; the button is exercised by integration
 * tests that assert the resulting DOM state.
 */

import type { ManualSyncResult } from "../drift/useManualSync";

export interface SyncButtonProps {
    sync: ManualSyncResult;
    /** Optional label override (defaults to "Sync to
     *  Host"). The drift indicator's Resync button
     *  uses its own label; the standalone button uses
     *  this. */
    label?: string;
}

export function SyncButton({ sync, label = "Sync to Host" }: SyncButtonProps): React.ReactNode {
    const onClick = () => {
        if (sync.isHost) {
            void sync.authoritativeSeek();
        } else {
            void sync.localSeek();
        }
    };
    let disabledReason: string | null = null;
    if (!sync.canSync) {
        if (sync.hostTargetMs === null) {
            disabledReason = "No host playback state available yet";
        } else {
            disabledReason = "Media not ready";
        }
    }
    return (
        <button
            type="button"
            className="sync-button"
            data-testid="sync-button"
            data-can-sync={sync.canSync ? "true" : "false"}
            onClick={onClick}
            disabled={!sync.canSync}
            aria-disabled={!sync.canSync}
            {...(disabledReason !== null ? { title: disabledReason } : {})}
        >
            {label}
        </button>
    );
}
