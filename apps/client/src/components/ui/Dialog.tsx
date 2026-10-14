import { useEffect, useId, useRef } from "react";
import type { KeyboardEvent, MouseEvent, ReactNode } from "react";
import { Button } from "./Button";

const FOCUSABLE =
    'a[href], button:not(:disabled), input:not(:disabled), select:not(:disabled), textarea:not(:disabled), [tabindex]:not([tabindex="-1"])';

/** Open dialogs, oldest first. Only the last one reacts to Escape and owns focus. */
const openDialogs: HTMLDialogElement[] = [];

interface DialogProps {
    title: string;
    children: ReactNode;
    /** Action buttons, rendered right-aligned in the footer. */
    footer?: ReactNode;
    /**
     * Called on Escape. Leave it out for a dialog that must not be dismissed
     * with the keyboard (the key is then swallowed so nothing behind reacts).
     */
    onEscape?: (() => void) | undefined;
    /** Shows a header close button that calls this. */
    onClose?: (() => void) | undefined;
    /** Disables the close button (an operation is running). */
    closeDisabled?: boolean;
    /** Called when the backdrop itself (not the panel) is clicked. Default: nothing. */
    onBackdropClick?: (() => void) | undefined;
    /** Optional description element id parts, wired to aria-describedby. */
    describedBy?: string | undefined;
    wide?: boolean;
    /** Extra class names for the panel and backdrop (kept for test and CSS hooks). */
    className?: string;
    backdropClassName?: string;
    testId?: string;
    backdropTestId?: string;
}

/**
 * The one modal dialog every flow uses.
 *
 * - Focus moves into the dialog on open (first control, or the panel), is kept
 *   inside while it is open, and returns to where it was on close.
 * - A click on the backdrop never reaches the panel's controls, and a click
 *   inside the panel never counts as a backdrop click.
 * - Escape only dismisses when the caller supplies `onEscape`.
 */
export function Dialog({
    title,
    children,
    footer,
    onEscape,
    onClose,
    closeDisabled = false,
    onBackdropClick,
    describedBy,
    wide = false,
    className,
    backdropClassName,
    testId,
    backdropTestId,
}: DialogProps): JSX.Element {
    const titleId = useId();
    const panelRef = useRef<HTMLDialogElement | null>(null);
    const onEscapeRef = useRef(onEscape);
    onEscapeRef.current = onEscape;

    useEffect(() => {
        const previous = document.activeElement instanceof HTMLElement ? document.activeElement : null;
        const panel = panelRef.current;
        if (panel !== null) {
            openDialogs.push(panel);
            const preferred = panel.querySelector<HTMLElement>("[data-autofocus]:not(:disabled)");
            const first = preferred ?? panel.querySelector<HTMLElement>(FOCUSABLE);
            (first ?? panel).focus();
        }
        const isTop = (): boolean => panel !== null && openDialogs[openDialogs.length - 1] === panel;
        const onDocKey = (e: globalThis.KeyboardEvent): void => {
            if (e.key === "Escape" && onEscapeRef.current !== undefined && isTop()) {
                e.preventDefault();
                onEscapeRef.current();
            }
        };
        // Focus that lands outside the topmost dialog (a click on the
        // backdrop, the focused button being removed, Tab from <body>) is
        // pulled back in: the page behind is not inert.
        const onFocusIn = (e: FocusEvent): void => {
            if (panel === null || !isTop()) return;
            const target = e.target;
            if (target instanceof Node && !panel.contains(target)) {
                const first = panel.querySelector<HTMLElement>(FOCUSABLE);
                (first ?? panel).focus();
            }
        };
        document.addEventListener("keydown", onDocKey);
        document.addEventListener("focusin", onFocusIn);
        return () => {
            document.removeEventListener("keydown", onDocKey);
            document.removeEventListener("focusin", onFocusIn);
            if (panel !== null) {
                const i = openDialogs.indexOf(panel);
                if (i !== -1) openDialogs.splice(i, 1);
            }
            if (previous !== null && previous.isConnected) previous.focus();
        };
    }, []);

    function onKeyDown(e: KeyboardEvent<HTMLDialogElement>): void {
        if (e.key === "Escape") {
            // Handled by the document listener when dismissable; swallowed otherwise.
            if (onEscapeRef.current === undefined) {
                e.preventDefault();
                e.stopPropagation();
            }
            return;
        }
        if (e.key !== "Tab") return;
        const panel = panelRef.current;
        if (panel === null) return;
        const items = [...panel.querySelectorAll<HTMLElement>(FOCUSABLE)].filter((el) => el.offsetParent !== null);
        if (items.length === 0) {
            e.preventDefault();
            panel.focus();
            return;
        }
        const first = items[0]!;
        const last = items[items.length - 1]!;
        const active = document.activeElement;
        if (e.shiftKey && (active === first || active === panel)) {
            e.preventDefault();
            last.focus();
        } else if (!e.shiftKey && active === last) {
            e.preventDefault();
            first.focus();
        }
    }

    function onBackdrop(e: MouseEvent<HTMLDivElement>): void {
        if (e.target !== e.currentTarget) return;
        onBackdropClick?.();
    }

    const panelClass = ["dialog", wide ? "dialog--wide" : "", className ?? ""].filter(Boolean).join(" ");
    const backdropClass = ["dialog-backdrop", backdropClassName ?? ""].filter(Boolean).join(" ");

    return (
        <div
            className={backdropClass}
            role="presentation"
            onClick={onBackdrop}
            {...(backdropTestId !== undefined ? { "data-testid": backdropTestId } : {})}
        >
            <dialog
                ref={panelRef}
                className={panelClass}
                open
                tabIndex={-1}
                aria-modal="true"
                aria-labelledby={titleId}
                {...(describedBy !== undefined ? { "aria-describedby": describedBy } : {})}
                onKeyDown={onKeyDown}
                {...(testId !== undefined ? { "data-testid": testId } : {})}
            >
                <div className="dialog__header">
                    <h2 id={titleId} className="dialog__title">
                        {title}
                    </h2>
                    {onClose !== undefined && (
                        <Button
                            variant="ghost"
                            size="sm"
                            iconOnly
                            icon="x"
                            aria-label="Close"
                            onClick={onClose}
                            disabled={closeDisabled}
                        />
                    )}
                </div>
                <div className="dialog__body">{children}</div>
                {footer !== undefined && <div className="dialog__footer">{footer}</div>}
            </dialog>
        </div>
    );
}
