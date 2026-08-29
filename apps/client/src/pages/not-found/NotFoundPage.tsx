import { Link } from "react-router-dom";

export function NotFoundPage(): JSX.Element {
    return (
        <div className="page-shell__content-inner">
            <p>Page not found.</p>
            <p>
                <Link to="/library">Back to library</Link>
            </p>
        </div>
    );
}
