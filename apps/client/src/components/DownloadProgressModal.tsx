import type { KeyboardEvent, MouseEvent } from "react";
import { useDownloadStore } from "../stores/useDownloadStore";
import type { DownloadProgressEvent, DownloadState } from "../services/downloads";
import "./DownloadProgressModal.css";

function stateLabel(s: DownloadState): string {
    switch (s) {
        case "pending": return "Preparing";
        case "connecting": return "Connecting to source";
        case "transferring": return "Downloading";
        case "verifying": return "Verifying";
        case "complete": return "Complete";
        case "failed": return "Failed";
        case "paused": return "Paused";
        case "cancelled": return "Cancelled";
    }
}

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

function shortId(id: string): string {
    if (id.length <= 8) return id;
    return id.slice(0, 8) + "…";
}

function pctOf(p: DownloadProgressEvent | undefined): number {
    if (!p || !Number.isFinite(p.total_bytes) || p.total_bytes <= 0) return 0;
    return Math.min(1, Math.max(0, p.transferred_bytes / p.total_bytes));
}

export function DownloadProgressModal(): JSX.Element | null {
    const active = useDownloadStore((s) => s.activeDownloads());
    if (active.length === 0) return null;
    const primary = active[0]!;

    const onKeyDown = (e: KeyboardEvent) => {
        if (e.key === "Escape") {
            e.preventDefault();
            e.stopPropagation();
        }
    };
    const onBackdropClick = (e: MouseEvent) => {
        e.preventDefault();
        e.stopPropagation();
    };

    const pct = pctOf(primary.progress);

    return (
        <div className="dlm-backdrop" data-testid="dlm-backdrop" onClick={onBackdropClick}>
            <dialog
                className="dlm-dialog"
                open
                aria-modal="true"
                aria-labelledby="dlm-title"
                aria-describedby="dlm-desc"
                onKeyDown={onKeyDown}
                data-testid="dlm-dialog"
            >
                <h2 id="dlm-title" className="dlm-title">
                    Download in progress
                </h2>
                <p id="dlm-desc" className="dlm-desc">
                    {stateLabel(primary.state)} media <code>{shortId(primary.mediaId)}</code>
                </p>
                <div
                    className="dlm-progress"
                    role="progressbar"
                    aria-valuemin={0}
                    aria-valuemax={100}
                    aria-valuenow={Math.round(pct * 100)}
                    data-testid="dlm-progress"
                >
                    <span style={{ width: `${pct * 100}%` }} />
                </div>
                <dl className="dlm-stats">
                    <dt>Transferred</dt>
                    <dd data-testid="dlm-transferred">
                        {humanBytes(primary.progress?.transferred_bytes ?? 0)} / {humanBytes(primary.progress?.total_bytes ?? 0)}
                    </dd>
                    <dt>Speed</dt>
                    <dd data-testid="dlm-rate">{humanRate(primary.progress?.bytes_per_sec_ema ?? 0)}</dd>
                    <dt>ETA</dt>
                    <dd data-testid="dlm-eta">{humanEta(primary.progress?.eta_seconds)}</dd>
                </dl>
                {primary.errorMessage && (
                    <p className="dlm-error" data-testid="dlm-error">{primary.errorMessage}</p>
                )}
                {active.length > 1 && (
                    <p className="dlm-multi">
                        {active.length - 1} more download{active.length - 1 === 1 ? "" : "s"} in progress.
                    </p>
                )}
            </dialog>
        </div>
    );
}
