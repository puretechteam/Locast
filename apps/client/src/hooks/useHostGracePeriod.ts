import { useEffect, useState } from "react";

export interface HostGracePeriodState {
    remainingMs: number | null;
    isExpired: boolean;
}

export function useHostGracePeriod(deadlineMs: number | null): HostGracePeriodState {
    const [remainingMs, setRemainingMs] = useState<number | null>(() =>
        deadlineMs !== null ? Math.max(0, deadlineMs - Date.now()) : null,
    );

    useEffect(() => {
        if (deadlineMs === null) {
            setRemainingMs(null);
            return;
        }

        setRemainingMs(Math.max(0, deadlineMs - Date.now()));

        const id = window.setInterval(() => {
            setRemainingMs(Math.max(0, deadlineMs - Date.now()));
        }, 1000);

        return () => window.clearInterval(id);
    }, [deadlineMs]);

    return {
        remainingMs,
        isExpired: remainingMs !== null && remainingMs <= 0,
    };
}
