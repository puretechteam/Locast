import { EmptyState, LinkButton } from "../../components/ui";

export function NotFoundPage(): JSX.Element {
    return (
        <div className="page page--narrow">
            <EmptyState
                icon="search"
                title="Page not found"
                headingLevel={1}
                actions={
                    <LinkButton to="/library" variant="primary" icon="film">
                        Back to library
                    </LinkButton>
                }
            >
                That address does not match anything in Locast.
            </EmptyState>
        </div>
    );
}
