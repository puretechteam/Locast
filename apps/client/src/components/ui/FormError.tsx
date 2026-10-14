import { Icon } from "./Icon";

/** A failed-submit message, shown inside the form it belongs to. */
export function FormError({ id, testId, children }: { id?: string; testId?: string; children: string }): JSX.Element {
    return (
        <p
            className="form__error"
            role="alert"
            {...(id !== undefined ? { id } : {})}
            {...(testId !== undefined ? { "data-testid": testId } : {})}
        >
            <Icon name="alert" />
            <span>{children}</span>
        </p>
    );
}
