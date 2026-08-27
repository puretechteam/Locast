import { StrictMode, useEffect, useState } from "react";
import { createRoot } from "react-dom/client";
import { commands } from "./services/ipc";

type Status = "loading" | "ready" | "error" | "browser";

function isTauriRuntime(): boolean {
  return (
    typeof window !== "undefined" &&
    typeof (window as unknown as { __TAURI_INTERNALS__?: unknown })
      .__TAURI_INTERNALS__ !== "undefined"
  );
}

function App(): JSX.Element {
  const [status, setStatus] = useState<Status>("loading");
  const [message, setMessage] = useState<string>("Hello, Locast");

  useEffect(() => {
    if (!isTauriRuntime()) {
      setStatus("browser");
      setMessage("Hello, Locast");
      return;
    }

    let cancelled = false;
    setStatus("loading");
    setMessage("Hello, Locast");

    commands
      .greet()
      .then((greeting) => {
        if (cancelled) return;
        setMessage(greeting);
        setStatus("ready");
      })
      .catch((err: unknown) => {
        if (cancelled) return;
        const detail = err instanceof Error ? err.message : String(err);
        setMessage(`Hello, Locast (greet failed: ${detail})`);
        setStatus("error");
      });

    return () => {
      cancelled = true;
    };
  }, []);

  return (
    <main
      style={{
        fontFamily:
          "system-ui, -apple-system, 'Segoe UI', Roboto, sans-serif",
        display: "flex",
        flexDirection: "column",
        alignItems: "center",
        justifyContent: "center",
        gap: "0.5rem",
        minHeight: "100vh",
        margin: 0,
        background: "#0b0d10",
        color: "#e6e6e6",
      }}
    >
      <h1 style={{ fontSize: "2rem", fontWeight: 500, margin: 0 }}>
        {message}
      </h1>
      <p
        style={{
          fontSize: "0.9rem",
          color: status === "error" ? "#f5a3a3" : "#9aa3ad",
          margin: 0,
        }}
      >
        {status === "loading" && "Calling greet()..."}
        {status === "ready" && "greet() returned successfully"}
        {status === "error" && "greet() failed; showing fallback"}
        {status === "browser" &&
          "Browser preview (Tauri runtime not detected)"}
      </p>
    </main>
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
