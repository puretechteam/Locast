import { useState } from "react";
import { errorText } from "../services/errors";
import { leaveRoom } from "../services/room";
import { resetRoomScopedStores } from "../stores/resetRoomScopedStores";
import { useDownloadStore } from "../stores/useDownloadStore";
import { useSharedMediaStore } from "../stores/useSharedMediaStore";
import type { DownloadProgressEvent, DownloadState } from "../services/downloads";
import { Button, Dialog, Icon, Spinner } from "./ui";
import "./DownloadProgressModal.css";

function stateLabel(s: DownloadState): string {
    switch (s) {
        case "pending": return "Waiting to start";
        case "connecting": return "Connecting to the host";
        case "transferring": return "Downloading";
        case "verifying": return "Checking the file";
        case "complete": return "Complete";
        case "failed": return "Failed";
        case "paused": return "Paused";
        case "cancelled": return "Cancelled";
    }
}

/** The stages a download moves through, for the step track. */
const STEPS: Array<{ label: string; states: DownloadState[] }> = [
    { label: "Connect", states: ["pending", "connecting"] },
    { label: "Download", states: ["transferring", "paused"] },
    { label: "Verify", states: ["verifying"] },
];

function humanBytes(n: number): string {
    if (!Number.isFinite(n) || n < 0) return "0 B";
    const units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let i = 0;
    let v = n;
    while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
    return `${v.toFixed(v >= 100 ? 0 : v >= 10 ? 1 : 2)} ${units[i]}`;
}

function humanRate(bps: number): string {
    if (!Number.isFinite(bps) || bps <= 0) return "—";
    return `${humanBytes(bps)}/s`;
}

function humanEta(seconds: number | null | undefined): string {
    if (seconds === null || seconds === undefined || !Number.isFinite(seconds) || seconds < 0) return "—";
    if (seconds < 60) return `${Math.round(seconds)}s`;
    if (seconds < 3600) return `${Math.floor(seconds / 60)}m ${Math.round(seconds % 60)}s`;
    return `${Math.floor(seconds / 3600)}h ${Math.floor((seconds % 3600) / 60)}m`;
}

function pctOf(p: DownloadProgressEvent | undefined): number {
    if (!p || !Number.isFinite(p.total_bytes) || p.total_bytes <= 0) return 0;
    return Math.min(1, Math.max(0, p.transferred_bytes / p.total_bytes));
}

export function DownloadProgressModal(): JSX.Element | null {
    const sharedMedia = useSharedMediaStore((s) => s.manifest?.media ?? null);
    const active = useDownloadStore((s) => s.activeDownloads());
    const dismiss = useDownloadStore((s) => s.dismiss);
    const [leaving, setLeaving] = useState(false);
    const [leaveError, setLeaveError] = useState<string | null>(null);
    if (active.length === 0) return null;
    const primary = active[0]!;
    // A failed download is not in progress: it stays visible so the error
    // can be read, but the user must be able to leave it.
    const failed = primary.state === "failed";
    // A download that has not started (no source is connected yet) is not
    // "downloading" either. The shared-media bridge keeps retrying while the
    // host's connection comes up, and may never succeed (for example behind a
    // restrictive NAT), so the user needs a way out: leaving the room.
    const notStarted = primary.state === "pending";

    async function leaveRoomInstead(): Promise<void> {
        if (leaving) return;
        setLeaving(true);
        setLeaveError(null);
        try {
            await leaveRoom();
            // Leaving on purpose emits no room event, so reset the room's
            // stores here. That also stops the shared-media bridge's retry
            // loop from raising this dialog again, and drops the stale room
            // summary the room page would otherwise show on the next visit.
            resetRoomScopedStores();
        } catch (err) {
            setLeaveError(errorText(err));
        } finally {
            setLeaving(false);
        }
    }

    const pct = pctOf(primary.progress);
    const filename = sharedMedia?.find((m) => m.id === primary.mediaId)?.filename ?? null;
    const unknownSize = primary.state === "pending" || primary.state === "connecting";
    const stepIndex = STEPS.findIndex((st) => st.states.includes(primary.state));

    return (
        <Dialog
            title={failed ? "Download failed" : "Receiving shared media"}
            describedBy="dlm-desc"
            className="dlm-dialog"
            backdropClassName="dlm-backdrop"
            backdropTestId="dlm-backdrop"
            testId="dlm-dialog"
            footer={
                failed ? (
                    <Button data-testid="dlm-dismiss" onClick={() => dismiss(primary.id)}>
                        Dismiss
                    </Button>
                ) : notStarted ? (
                    <Button
                        variant="danger"
                        icon="leave"
                        data-testid="dlm-leave"
                        disabled={leaving}
                        onClick={() => void leaveRoomInstead()}
                    >
                        {leaving ? "Leaving..." : "Leave room"}
                    </Button>
                ) : undefined
            }
        >
            <p id="dlm-desc" className="dlm-desc">
                <span className="dlm-state">
                    {failed ? <Icon name="alert" /> : <Spinner />}
                    {stateLabel(primary.state)}
                </span>
                {filename !== null && (
                    <span className="dlm-file" title={filename}>
                        {filename}
                    </span>
                )}
            </p>
            {!failed && stepIndex >= 0 && (
                <ol className="dlm-steps" aria-label="Stages">
                    {STEPS.map((st, i) => (
                        <li
                            key={st.label}
                            className={`dlm-step${i < stepIndex ? " dlm-step--done" : ""}${i === stepIndex ? " dlm-step--current" : ""}`}
                            {...(i === stepIndex ? { "aria-current": "step" as const } : {})}
                        >
                            {st.label}
                        </li>
                    ))}
                </ol>
            )}
            <div
                className={`progress${unknownSize ? " progress--indeterminate" : ""}`}
                role="progressbar"
                aria-label="Download progress"
                aria-valuemin={0}
                aria-valuemax={100}
                aria-valuenow={Math.round(pct * 100)}
                data-testid="dlm-progress"
            >
                <span style={unknownSize ? undefined : { width: `${pct * 100}%` }} />
            </div>
            <dl className="dlm-stats">
                <dt>Transferred</dt>
                <dd data-testid="dlm-transferred">
                    {humanBytes(primary.progress?.transferred_bytes ?? 0)} / {humanBytes(primary.progress?.total_bytes ?? 0)}
                </dd>
                <dt>Speed</dt>
                <dd data-testid="dlm-rate">{failed ? "—" : humanRate(primary.progress?.bytes_per_sec_ema ?? 0)}</dd>
                <dt>Time left</dt>
                <dd data-testid="dlm-eta">{failed ? "—" : humanEta(primary.progress?.eta_seconds)}</dd>
            </dl>
            {primary.errorMessage && (
                <p className="dialog__error" data-testid="dlm-error">{primary.errorMessage}</p>
            )}
            {active.length > 1 && (
                <p className="dlm-multi">
                    {active.length - 1} more download{active.length - 1 === 1 ? "" : "s"} in progress.
                </p>
            )}
            {notStarted && (
                <>
                    <p className="dlm-multi" data-testid="dlm-waiting">
                        Waiting for the host to connect. You can leave the room instead.
                    </p>
                    {leaveError !== null && (
                        <p className="dialog__error" role="alert" data-testid="dlm-leave-error">
                            {leaveError}
                        </p>
                    )}
                </>
            )}
        </Dialog>
    );
}
