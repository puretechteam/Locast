# Locast

**Status: pre-implementation, design complete.**

Locast is a local-first watch-together desktop app. A host picks one or more media files in their library, generates a room code, and shares it out of band; viewers join the code, the host's files transfer to them over a peer-to-peer WebRTC DataChannel (verified end-to-end against the host's Ed25519 signature), and playback happens locally on each participant's machine against their own copy. The local `<video>` element does the decoding; the server only relays control and presence.

The full design lives in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md). The phased implementation plan lives in [`docs/ROADMAP.md`](docs/ROADMAP.md). The docs folder is indexed at [`docs/README.md`](docs/README.md).

This repository is not yet implementing code. The two documents above are the deliverables of the design phase.
