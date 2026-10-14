import type { ReactNode } from "react";

export type PillTone = "neutral" | "accent" | "success" | "warning" | "danger";

interface StatusPillProps {
    tone?: PillTone;
    /** A leading dot, for live states. */
    dot?: boolean;
    pulse?: boolean;
    children: ReactNode;
    title?: string;
    testId?: string;
}

/** A small rounded status label. Always carries text, never color alone. */
export function StatusPill({
    tone = "neutral",
    dot = false,
    pulse = false,
    children,
    title,
    testId,
}: StatusPillProps): JSX.Element {
    const cls = ["pill", tone === "neutral" ? "" : `pill--${tone}`, pulse ? "pill--pulse" : ""]
        .filter(Boolean)
        .join(" ");
    return (
        <span
            className={cls}
            {...(title !== undefined ? { title } : {})}
            {...(testId !== undefined ? { "data-testid": testId } : {})}
        >
            {dot && <span className="pill__dot" aria-hidden="true" />}
            {children}
        </span>
    );
}
