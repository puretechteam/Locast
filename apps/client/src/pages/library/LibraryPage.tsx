import { Link } from "react-router-dom";

export function LibraryPage(): JSX.Element {
    return (
        <div className="page-shell__content-inner">
            <p>Your library is empty.</p>
            <p>
                <Link to="/rooms/new">Create a room</Link> or{" "}
                <Link to="/rooms/join">join one</Link> to start watching.
            </p>
        </div>
    );
}
