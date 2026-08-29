import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { BrowserRouter, Navigate, Route, Routes } from "react-router-dom";
import { PageShell } from "./components/PageShell";
import { LibraryPage } from "./pages/library";
import { NotFoundPage } from "./pages/not-found";
import { RoomPage } from "./pages/rooms.$id";
import { JoinRoomPage } from "./pages/rooms.join";
import { NewRoomPage } from "./pages/rooms.new";
import { RoomsIndexPage } from "./pages/rooms.index";
import "./styles/room.css";

function App(): JSX.Element {
    return (
        <BrowserRouter>
            <Routes>
                <Route path="/" element={<Navigate to="/library" replace />} />
                <Route
                    path="/library"
                    element={
                        <PageShell title="Library">
                            <LibraryPage />
                        </PageShell>
                    }
                />
                <Route
                    path="/rooms"
                    element={
                        <PageShell title="Rooms">
                            <RoomsIndexPage />
                        </PageShell>
                    }
                />
                <Route
                    path="/rooms/new"
                    element={
                        <PageShell title="New room">
                            <NewRoomPage />
                        </PageShell>
                    }
                />
                <Route
                    path="/rooms/join"
                    element={
                        <PageShell title="Join room">
                            <JoinRoomPage />
                        </PageShell>
                    }
                />
                <Route
                    path="/rooms/:id"
                    element={
                        <PageShell title="Room">
                            <RoomPage />
                        </PageShell>
                    }
                />
                <Route path="*" element={<NotFoundPage />} />
            </Routes>
        </BrowserRouter>
    );
}

const container = document.getElementById("root");
if (!container) {
    throw new Error("Locast: #root element not found");
}

createRoot(container).render(
    <StrictMode>
        <App />
    </StrictMode>,
);
