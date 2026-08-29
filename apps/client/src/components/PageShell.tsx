import type { ReactNode } from "react";

interface PageShellProps {
    title: string;
    children: ReactNode;
}

export function PageShell({ title, children }: PageShellProps): JSX.Element {
    return (
        <div className="page-shell">
            <header className="page-shell__header">
                <h1 className="page-shell__title">{title}</h1>
            </header>
            <main className="page-shell__content">{children}</main>
        </div>
    );
}
