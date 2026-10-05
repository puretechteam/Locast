import { Component } from "react";
import type { ErrorInfo, ReactNode } from "react";

interface ErrorBoundaryProps {
    children: ReactNode;
}

interface ErrorBoundaryState {
    error: Error | null;
}

/**
 * Catches an exception thrown while rendering any component below it. Without
 * one, React unmounts the whole tree and the window goes blank with nothing to
 * click, and the only recovery is to restart the app.
 *
 * The fallback offers a reload and a plain link to the library. The link is a
 * normal anchor on purpose: it reloads the page, so the failed React tree and
 * the in-memory stores that may have caused the failure are discarded.
 */
export class ErrorBoundary extends Component<ErrorBoundaryProps, ErrorBoundaryState> {
    override state: ErrorBoundaryState = { error: null };

    static getDerivedStateFromError(error: unknown): ErrorBoundaryState {
        return { error: error instanceof Error ? error : new Error(String(error)) };
    }

    override componentDidCatch(error: Error, info: ErrorInfo): void {
        console.error("ErrorBoundary: a component failed to render", error, info.componentStack);
    }

    override render(): ReactNode {
        if (this.state.error === null) return this.props.children;
        return (
            <div className="error-boundary" role="alert" data-testid="error-boundary">
                <h1>Something went wrong</h1>
                <p>
                    Locast hit an unexpected problem and could not show this page. Reloading
                    usually fixes it.
                </p>
                <p className="error-boundary__detail" data-testid="error-boundary-detail">
                    {this.state.error.message}
                </p>
                <p>
                    <button type="button" onClick={() => window.location.reload()}>
                        Reload
                    </button>{" "}
                    <a href="/library">Back to library</a>
                </p>
            </div>
        );
    }
}
