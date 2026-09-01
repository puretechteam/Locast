import type { ReactNode } from "react";
import { useDownloadStore } from "../stores/useDownloadStore";

export function DownloadBlockingGuard({ children }: { children: ReactNode }): JSX.Element | null {
    const blocked = useDownloadStore((s) => s.hasActiveDownload());
    if (blocked) return null;
    return <>{children}</>;
}
