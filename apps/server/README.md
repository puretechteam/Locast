# Locast signaling server

This crate is the Locast signaling and room coordination server. P0-T01 registers it as a Cargo workspace member so the workspace compiles; P0-T03 (server skeleton) adds `axum` 0.7, the `/health` and `/version` HTTP endpoints, and the `Dockerfile` / `docker-compose.dev.yml`. P2+ adds the WebSocket endpoint, the auth handshake, the room registry, presence, and rate limiting per `docs/ARCHITECTURE.md` section 26.3.
