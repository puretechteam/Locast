import type { ReactNode } from "react";
import { Link } from "react-router-dom";
import { Icon } from "./Icon";

interface PageHeaderProps {
    title: string;
    description?: ReactNode;
    /** Primary and secondary page actions, right-aligned. */
    actions?: ReactNode;
    /** A "back" link shown above the title. */
    back?: { to: string; label: string } | undefined;
}

/** The page's single `<h1>`, an optional one-line explanation, and its actions. */
export function PageHeader({ title, description, actions, back }: PageHeaderProps): JSX.Element {
    return (
        <header className="page-header">
            <div className="page-header__text">
                {back !== undefined && (
                    <Link to={back.to} className="page-header__back">
                        <Icon name="arrow-left" />
                        {back.label}
                    </Link>
                )}
                <h1 className="page-header__title">{title}</h1>
                {description !== undefined && <p className="page-header__desc">{description}</p>}
            </div>
            {actions !== undefined && <div className="page-header__actions">{actions}</div>}
        </header>
    );
}
