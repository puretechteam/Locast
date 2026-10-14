import type { ReactNode } from "react";
import { Icon } from "./Icon";
import type { IconName } from "./Icon";

export type NoticeKind = "info" | "success" | "warning" | "error";

const ICONS: Record<NoticeKind, IconName> = {
    info: "info",
    success: "check",
    warning: "alert",
    error: "alert",
};

interface NoticeProps {
    kind?: NoticeKind;
    title?: string;
    children: ReactNode;
    actions?: ReactNode;
    /** Overrides the default live-region role (error: alert, others: status). */
    role?: "alert" | "status" | "none";
    testId?: string;
}

/** An inline message with an icon, an optional title and optional actions. */
export function Notice({ kind = "info", title, children, actions, role, testId }: NoticeProps): JSX.Element {
    const resolved = role ?? (kind === "error" ? "alert" : "status");
    return (
        <div
            className={`notice notice--${kind}`}
            {...(resolved === "none" ? {} : { role: resolved })}
            {...(testId !== undefined ? { "data-testid": testId } : {})}
        >
            <Icon name={ICONS[kind]} className="notice__icon" />
            <div className="notice__body">
                {title !== undefined && <strong className="notice__title">{title}</strong>}
                <span>{children}</span>
            </div>
            {actions !== undefined && <div className="notice__actions">{actions}</div>}
        </div>
    );
}
