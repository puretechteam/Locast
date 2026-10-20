import type { ReactNode } from "react";

/**
 * Inline stroke icons (24 x 24 grid). One small set, drawn in one style, so the
 * app does not need an icon dependency. Icons are decorative: pair them with
 * visible text or give the button an `aria-label`.
 */
const PATHS = {
    alert: (
        <>
            <path d="M12 3.5 2.8 19.5h18.4z" />
            <path d="M12 10v4M12 16.9v.1" />
        </>
    ),
    "arrow-left": <path d="M19 12H5M11 6l-6 6 6 6" />,
    chat: <path d="M4 5h16a1 1 0 0 1 1 1v10a1 1 0 0 1-1 1h-8l-5 4v-4H4a1 1 0 0 1-1-1V6a1 1 0 0 1 1-1z" />,
    check: <path d="m5 12.5 4.5 4.5L19 7.5" />,
    clock: (
        <>
            <circle cx="12" cy="12" r="9" />
            <path d="M12 7v5l3 2" />
        </>
    ),
    copy: (
        <>
            <rect x="9" y="9" width="11" height="11" rx="2" />
            <path d="M5 15a1 1 0 0 1-1-1V5a1 1 0 0 1 1-1h9a1 1 0 0 1 1 1" />
        </>
    ),
    download: <path d="M12 4v11M7.5 11.5 12 16l4.5-4.5M4 19h16" />,
    film: (
        <>
            <rect x="3" y="3" width="18" height="18" rx="2" />
            <path d="M7 3v18M17 3v18M3 8h4M3 12h4M3 16h4M17 8h4M17 12h4M17 16h4" />
        </>
    ),
    info: (
        <>
            <circle cx="12" cy="12" r="9" />
            <path d="M12 11v5M12 7.9v.1" />
        </>
    ),
    join: <path d="M15 4h4a1 1 0 0 1 1 1v14a1 1 0 0 1-1 1h-4M10 8l4 4-4 4M14 12H4" />,
    leave: <path d="M9 4H5a1 1 0 0 0-1 1v14a1 1 0 0 0 1 1h4M16 8l4 4-4 4M20 12H9" />,
    link: <path d="M10 14a4 4 0 0 0 5.7 0l3-3a4 4 0 0 0-5.7-5.7l-1 1M14 10a4 4 0 0 0-5.7 0l-3 3a4 4 0 0 0 5.7 5.7l1-1" />,
    laser: (
        <>
            <circle cx="12" cy="12" r="7" />
            <circle cx="12" cy="12" r="1.5" />
            <path d="M12 2v3M12 19v3M2 12h3M19 12h3" />
        </>
    ),
    pause: <path d="M8 5v14M16 5v14" />,
    pen: (
        <>
            <path d="M4 20l1-4L16.5 4.5a2.1 2.1 0 0 1 3 3L8 19z" />
            <path d="M14.5 6.5l3 3" />
        </>
    ),
    play: <path d="M7 4.5v15l12-7.5z" />,
    plus: <path d="M12 5v14M5 12h14" />,
    refresh: <path d="M20 11a8 8 0 1 0-2.3 5.7M20 4v7h-7" />,
    search: (
        <>
            <circle cx="11" cy="11" r="7" />
            <path d="m21 21-4.3-4.3" />
        </>
    ),
    settings: (
        <>
            <path d="M4 7h9M19 7h1M4 17h1M11 17h9" />
            <circle cx="16" cy="7" r="2.5" />
            <circle cx="8" cy="17" r="2.5" />
        </>
    ),
    shield: <path d="M12 3 4.5 6v6c0 4.5 3 7.5 7.5 9 4.5-1.5 7.5-4.5 7.5-9V6z" />,
    trash: <path d="M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13M10 11v6M14 11v6" />,
    upload: <path d="M12 15V4M7.5 8.5 12 4l4.5 4.5M4 15v4a1 1 0 0 0 1 1h14a1 1 0 0 0 1-1v-4" />,
    users: (
        <>
            <circle cx="9" cy="8" r="3.5" />
            <path d="M2.5 20a6.5 6.5 0 0 1 13 0M16 4.6a3.5 3.5 0 0 1 0 6.8M18 14.2a6.5 6.5 0 0 1 3.5 5.8" />
        </>
    ),
    x: <path d="M6 6l12 12M18 6 6 18" />,
} satisfies Record<string, ReactNode>;

export type IconName = keyof typeof PATHS;

export function Icon({ name, className }: { name: IconName; className?: string }): JSX.Element {
    return (
        <svg
            className={className === undefined ? "icon" : `icon ${className}`}
            viewBox="0 0 24 24"
            aria-hidden="true"
            focusable="false"
        >
            {PATHS[name]}
        </svg>
    );
}
