import type { ButtonHTMLAttributes, ReactNode } from "react";
import { Link } from "react-router-dom";
import type { LinkProps } from "react-router-dom";
import { Icon } from "./Icon";
import type { IconName } from "./Icon";

export type ButtonVariant = "primary" | "secondary" | "ghost" | "danger" | "danger-solid";

interface Look {
    variant?: ButtonVariant | undefined;
    size?: "md" | "sm" | undefined;
    icon?: IconName | undefined;
    /** Icon-only button: `children` is not rendered, so give it an `aria-label`. */
    iconOnly?: boolean | undefined;
    block?: boolean | undefined;
}

function classes(look: Look, extra: string | undefined): string {
    const parts = ["btn"];
    if (look.variant !== undefined && look.variant !== "secondary") parts.push(`btn--${look.variant}`);
    if (look.size === "sm") parts.push("btn--sm");
    if (look.iconOnly === true) parts.push("btn--icon");
    if (look.block === true) parts.push("btn--block");
    if (extra !== undefined) parts.push(extra);
    return parts.join(" ");
}

export interface ButtonProps extends Look, Omit<ButtonHTMLAttributes<HTMLButtonElement>, "children"> {
    children?: ReactNode;
}

/** A real `<button>`. Defaults to `type="button"` so it never submits a form by accident. */
export function Button({
    variant,
    size,
    icon,
    iconOnly,
    block,
    className,
    type = "button",
    children,
    ...rest
}: ButtonProps): JSX.Element {
    return (
        <button type={type} className={classes({ variant, size, iconOnly, block }, className)} {...rest}>
            {icon !== undefined && <Icon name={icon} />}
            {iconOnly === true ? null : children}
        </button>
    );
}

export interface LinkButtonProps extends Look, LinkProps {}

/** A real router link that looks like a button. Navigation stays a link. */
export function LinkButton({
    variant,
    size,
    icon,
    iconOnly,
    block,
    className,
    children,
    ...rest
}: LinkButtonProps): JSX.Element {
    return (
        <Link className={classes({ variant, size, iconOnly, block }, className)} {...rest}>
            {icon !== undefined && <Icon name={icon} />}
            {iconOnly === true ? null : children}
        </Link>
    );
}
