import { useEffect } from "react";
import type { ReactNode } from "react";
import { Link, NavLink } from "react-router-dom";
import { useRoomStore } from "../stores/useRoomStore";
import { Icon } from "./ui";
import "../styles/shell.css";

interface PageShellProps {
    /** Names the page in the window title; the visible `<h1>` is the page's own `PageHeader`. */
    title: string;
    /**
     * `page` (default) scrolls normally. `fill` hands the page the whole area
     * below the header and lets it manage its own scrolling (the room).
     */
    layout?: "page" | "fill";
    children: ReactNode;
}

function BrandMark(): JSX.Element {
    return (
        <svg className="app-header__mark" viewBox="0 0 24 24" aria-hidden="true" focusable="false">
            <rect width="24" height="24" rx="6" fill="var(--accent)" />
            <path d="M9.5 7.2v9.6l7.6-4.8z" fill="#fff" />
        </svg>
    );
}

/**
 * The application frame: a compact header with the brand, primary navigation
 * (Library, Rooms), a way back into the current room, and Settings. Every
 * screen is rendered inside it so the user always knows where they are.
 */
export function PageShell({ title, layout = "page", children }: PageShellProps): JSX.Element {
    const summary = useRoomStore((s) => s.summary);

    useEffect(() => {
        document.title = `${title} - Locast`;
    }, [title]);

    return (
        <div className="app-shell">
            <header className="app-header">
                <Link to="/library" className="app-header__brand" aria-label="Locast home">
                    <BrandMark />
                    <span className="app-header__name">Locast</span>
                </Link>
                <nav className="app-header__nav" aria-label="Primary">
                    <NavLink to="/library" className="app-header__link">
                        <Icon name="film" />
                        Library
                    </NavLink>
                    <NavLink to="/rooms" className="app-header__link">
                        <Icon name="users" />
                        Rooms
                    </NavLink>
                </nav>
                <div className="app-header__end">
                    {summary !== null && (
                        <Link
                            to={`/rooms/${summary.id}`}
                            className="app-header__room"
                            title={`Return to ${summary.title}`}
                            data-testid="header-room-link"
                        >
                            <span className="app-header__room-dot" aria-hidden="true" />
                            <span className="app-header__room-title">{summary.title}</span>
                        </Link>
                    )}
                    <NavLink to="/settings" className="app-header__link">
                        <Icon name="settings" />
                        Settings
                    </NavLink>
                </div>
            </header>
            <main className={`app-main app-main--${layout}`}>{children}</main>
        </div>
    );
}
