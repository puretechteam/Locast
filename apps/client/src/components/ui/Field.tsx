import { useId } from "react";
import type { ReactNode } from "react";

interface FieldProps {
    label: string;
    hint?: ReactNode;
    error?: ReactNode;
    optional?: boolean;
    /** Render-prop: receives the ids to wire onto the actual control. */
    children: (ids: { id: string; describedBy: string | undefined }) => ReactNode;
}

/**
 * A labelled form control with helper text and an error line. The label is a
 * real `<label for>`, and the hint and error are tied to the control through
 * `aria-describedby`.
 */
export function Field({ label, hint, error, optional = false, children }: FieldProps): JSX.Element {
    const id = useId();
    const hintId = `${id}-hint`;
    const errorId = `${id}-error`;
    const hasError = error !== undefined && error !== null;
    const describedBy =
        [hint !== undefined ? hintId : null, hasError ? errorId : null]
            .filter((x): x is string => x !== null)
            .join(" ") || undefined;
    return (
        <div className="field">
            <label className="field__label" htmlFor={id}>
                {label}
                {optional && <span className="field__optional"> (optional)</span>}
            </label>
            {children({ id, describedBy })}
            {hint !== undefined && (
                <span className="field__hint" id={hintId}>
                    {hint}
                </span>
            )}
            {hasError && (
                <span className="field__error" id={errorId} role="alert">
                    {error}
                </span>
            )}
        </div>
    );
}
