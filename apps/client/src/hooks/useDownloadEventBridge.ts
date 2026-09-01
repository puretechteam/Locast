import { useEffect } from "react";
import { onDownloadProgress, onDownloadState } from "../services/downloads";
import { useDownloadStore } from "../stores/useDownloadStore";

export function DownloadEventBridge(): null {
    useEffect(() => {
        let cancelled = false;
        const unsubs: Array<() => void> = [];
        (async () => {
            try {
                const u1 = await onDownloadState((e) => {
                    if (cancelled) return;
                    useDownloadStore.getState().setState(e);
                });
                if (cancelled) { u1(); return; }
                unsubs.push(u1);
                const u2 = await onDownloadProgress((e) => {
                    if (cancelled) return;
                    useDownloadStore.getState().setProgress(e);
                });
                if (cancelled) { u2(); return; }
                unsubs.push(u2);
                if (typeof window !== "undefined") {
                    (window as unknown as { __locast_subscribed?: boolean }).__locast_subscribed = true;
                }
            } catch (err) {
                console.warn("DownloadEventBridge: listen failed", err);
            }
        })();
        return () => {
            cancelled = true;
            for (const u of unsubs) {
                try { u(); } catch { /* swallow */ }
            }
        };
    }, []);
    return null;
}
