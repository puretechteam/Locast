import { useEffect, useRef } from "react";
import { commands } from "../services/ipc";
import {
    useConnectionQualityStore,
    classifyQuality,
} from "../stores/useConnectionQualityStore";

const PROBE_INTERVAL_MS = 1_000;

export function useConnectionQuality(): void {
    const lastProbeRef = useRef<number>(0);

    useEffect(() => {
        let stopped = false;

        const probe = async (): Promise<void> => {
            if (stopped) return;
            const now = Date.now();
            if (now - lastProbeRef.current < PROBE_INTERVAL_MS) return;
            lastProbeRef.current = now;

            try {
                const sample = await commands.clockSkewProbe();
                if (stopped) return;
                const rttMs = sample.t3_local_ms - sample.t0_local_ms;
                if (rttMs < 0) return;
                const quality = classifyQuality(rttMs);
                useConnectionQualityStore.getState().setQuality(quality, rttMs);
            } catch {
                useConnectionQualityStore.getState().setQuality("poor", -1);
            }
        };

        void probe();
        const id = window.setInterval(() => {
            void probe();
        }, PROBE_INTERVAL_MS);

        return () => {
            stopped = true;
            window.clearInterval(id);
        };
    }, []);
}
