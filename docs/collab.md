# Encrypted collaboration

`dial relay --bind 127.0.0.1:7331` runs an in-memory WebSocket relay. Configure `[collab].relay_url` to its `ws://` or `wss://` origin. Hosts share a link containing a random room key, and browsers and guests use AES-256-GCM to open encrypted frames. The relay forwards opaque frame bytes and never receives the key or transcript plaintext.

Two link forms carry authenticated key material, and neither sends it to the relay. The
application link printed by `/collab` is `host[:port]/r/<roomId>.<key-material>`: Dial removes
the key material before connecting to `wss://host/r/<roomId>`. The browser viewer wraps that
application link in a URL fragment, `https://viewer.example/#<link>`, which is not sent to the
viewer server. View links contain the room encryption key and host verification key; full links
also contain the write token. Keep both forms private.

The protocol is version **1004**. Host frames are signed and verified against the host key in
the link. Writable guests prove possession of the write token using a fresh host challenge and
authenticate each mutation; the raw token is never sent in `hello` or other wire frames. A
view-link holder cannot authenticate host frames or gain write authority by claiming the host
role at the relay. Older unsigned links fail closed: regenerate links and update terminal and
browser clients together. Transport role, epoch and sequence replay checks remain in force.

**Relay trust assumption:** the relay is trusted for availability, not for integrity. It can drop, delay or close rooms, and it sees metadata such as room IDs, peer IDs, frame sizes and timing. It is not trusted to deliver authentic or fresh frames: a captured frame that the relay redelivers is rejected by every receiver that already processed it, and a host→guest frame cannot be reflected back as a guest command. Replay state lives as long as the local socket owner (a host's state survives reconnects and room recreation, and a restarted host also has a fresh room key). A restarted guest starts with empty replay state, so it can display history it already showed; host→guest frames only update the display, and guest→host commands cannot be replayed into a host that has not restarted.

The static browser viewer lives in `assets/collab-web/` and can be hosted separately over HTTPS. The viewer reconnects after transient drops, rebuilds its view from each complete snapshot, and reduces frames one at a time in arrival order. Keep full links private: possession of the key grants read access. Guest write/steering permission remains a host-side decision; do not expose an unauthenticated relay to untrusted networks.

A guest joined from the terminal sees the replicated conversation in its transcript, each entry once. A writable guest's prompts are sent to the host agent (`/collab abort` asks the host to stop its running turn); a read-only guest's prompts are refused, and no guest prompt runs on the guest's local agent. The host bounds guest steering: at most 8 queued prompts totalling 256 KiB, 64 KiB per prompt, with aborts coalesced into one. Excess requests get an explicit error reply.

Snapshots for late joiners share the host's log rather than copying it. A peer has at most one unfinished snapshot, a new hello supersedes it, and queued snapshots reference at most 64 MiB of distinct history.

Each connection owns its socket, so an idle reader cannot block outbound broadcasts. Binary messages are limited to 8 MiB; each recipient has a bounded 16-frame queue and a 10-second stalled-write deadline. An overloaded recipient is disconnected without disconnecting a healthy broadcasting host. Guest envelope peer IDs are replaced with the assigned sender ID. Host disconnect sends `room-closed` to guests and releases room membership, including on transport errors.

The relay admits at most 64 connections (including incomplete handshakes) and 16 rooms.
Connection-worker startup failures are logged without ending the listening service. A
non-loopback bind warns about exposure; these limits do not replace a reverse proxy's admission
controls or make an unauthenticated public relay safe against availability attacks.

The built-in relay does not terminate TLS. For remote deployment, put it behind a TLS reverse proxy and use `wss://`; apply connection and resource limits there. Plain `ws://` links are restricted to loopback hosts.
