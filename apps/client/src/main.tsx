import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { BrowserRouter, Navigate, Route, Routes } from "react-router-dom";
import { ErrorBoundary } from "./components/ErrorBoundary";
import { PageShell } from "./components/PageShell";
import { DownloadBlockingGuard } from "./components/DownloadBlockingGuard";
import { DownloadProgressModal } from "./components/DownloadProgressModal";
import { DownloadEventBridge } from "./hooks/useDownloadEventBridge";
import { ChatBridge } from "./hooks/useChatBridge";
import { RoomEndBridge } from "./hooks/useRoomEndBridge";
import { SharedMediaBridge } from "./hooks/useSharedMediaBridge";
import { LibraryPage } from "./pages/library";
import { NotFoundPage } from "./pages/not-found";
import { RoomPage } from "./pages/rooms.$id";
import { JoinRoomPage } from "./pages/rooms.join";
import { NewRoomPage } from "./pages/rooms.new";
import { RoomsIndexPage } from "./pages/rooms.index";
import { SettingsPage } from "./pages/settings";
import "./styles/room.css";

function App(): JSX.Element {
    return (
        <BrowserRouter>
            <DownloadEventBridge />
            <SharedMediaBridge />
            <RoomEndBridge />
            <ChatBridge />
            <Routes>
                <Route path="/" element={<Navigate to="/library" replace />} />
                <Route path="/library" element={<PageShell title="Library"><LibraryPage /></PageShell>} />
                <Route path="/settings" element={<PageShell title="Settings"><SettingsPage /></PageShell>} />
                <Route path="/rooms" element={<PageShell title="Rooms"><RoomsIndexPage /></PageShell>} />
                <Route path="/rooms/new" element={<PageShell title="New room"><NewRoomPage /></PageShell>} />
                <Route path="/rooms/join" element={<PageShell title="Join room"><JoinRoomPage /></PageShell>} />
                <Route
                    path="/rooms/:id"
                    element={
                        <DownloadBlockingGuard>
                            <PageShell title="Room"><RoomPage /></PageShell>
                        </DownloadBlockingGuard>
                    }
                />
                <Route path="*" element={<NotFoundPage />} />
            </Routes>
            <DownloadProgressModal />
        </BrowserRouter>
    );
}

const container = document.getElementById("root");
if (!container) {
    throw new Error("Locast: #root element not found");
}
createRoot(container).render(
    <StrictMode>
        <ErrorBoundary>
            <App />
        </ErrorBoundary>
    </StrictMode>,
);
