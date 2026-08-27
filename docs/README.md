# Locast docs

This folder holds the documentation that defines Locast.

## Authoritative documents

- **[ARCHITECTURE.md](ARCHITECTURE.md)** - the canonical architecture. Sections 1-30 cover the stack, the client, storage, the manifest, downloads, rooms, playback, drawing, networking, WebRTC, the server, security, reconnection, media lifecycle, UI, project structure, testing, performance, risks, and deferred items. This is the source of truth; the implementation must conform to it.
- **[ROADMAP.md](ROADMAP.md)** - the atomic, phased implementation plan. 10 phases (P0-P9), 70 tasks. Each task is sized for a single focused coding session and is sequenced by prerequisites. Tasks map back to the architecture sections they implement.

## Stub placeholders (not yet written)

- **[SECURITY.md](SECURITY.md)** - reserved for the v1 security policy: how to report a vulnerability, supported versions, the threat model summary, and a pointer to the architecture's section 21.
- **[CONTRIBUTING.md](CONTRIBUTING.md)** - reserved for the contribution guide: repo layout, development setup, the pull-request flow, the Definition of Done, and the coding conventions that go beyond `AGENTS.md`.
- **[DEPLOYMENT.md](DEPLOYMENT.md)** - reserved for the operator's guide to deploying the signaling server: Docker, coturn, Caddy, environment variables, the Prometheus metrics, the audit log retention policy, and the backup story.

These three files are intentionally stubs. They will be filled in as the corresponding phases (P0-P9) of the roadmap land.

## Other references

- `AGENTS.md` (repo root) - the agent rule book for this project; in addition to the cross-project `AGENTS.md` that sits in this project's parent directory (e.g., `../AGENTS.md`).
- The original design drafts are under `design/drafts/`. They are not authoritative; the canonical versions live in `ARCHITECTURE.md`.
