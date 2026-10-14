import type { ReactNode } from "react";
import { Icon } from "./Icon";
import type { IconName } from "./Icon";

interface EmptyStateProps {
    icon: IconName;
    title: string;
    children?: ReactNode;
    actions?: ReactNode;
    tone?: "default" | "error";
    plain?: boolean;
    /** Icon beside the text instead of stacked above it. */
    compact?: boolean;
    testId?: string;
    role?: "alert" | "status";
    /** Heading level; defaults to h2. */
    headingLevel?: 1 | 2 | 3;
}

/** A compact, balanced empty or error state: icon, title, a sentence and actions. */
export function EmptyState({
    icon,
    title,
    children,
    actions,
    tone = "default",
    plain = false,
    compact = false,
    testId,
    role,
    headingLevel = 2,
}: EmptyStateProps): JSX.Element {
    const cls = ["empty", plain ? "empty--plain" : "", compact ? "empty--compact" : "", tone === "error" ? "empty--error" : ""]
        .filter(Boolean)
        .join(" ");
    const Heading = headingLevel === 1 ? "h1" : headingLevel === 2 ? "h2" : "h3";
    return (
        <div
            className={cls}
            {...(testId !== undefined ? { "data-testid": testId } : {})}
            {...(role !== undefined ? { role } : {})}
        >
            <span className="empty__icon">
                <Icon name={icon} />
            </span>
            <Heading className="empty__title">{title}</Heading>
            {children !== undefined && <div className="empty__desc">{children}</div>}
            {actions !== undefined && <div className="empty__actions">{actions}</div>}
        </div>
    );
}
