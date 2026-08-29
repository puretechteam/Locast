import { Link } from "react-router-dom";

export function RoomsIndexPage(): JSX.Element {
    return (
        <div className="page-shell__content-inner">
            <ul className="rooms-index__list">
                <li>
                    <Link to="/rooms/new">Create room</Link>
                </li>
                <li>
                    <Link to="/rooms/join">Join room</Link>
                </li>
                <li>
                    <Link to="/library">Back to library</Link>
                </li>
            </ul>
        </div>
    );
}
