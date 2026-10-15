# Encrypted collaboration

`dial relay --bind 127.0.0.1:7331` runs an in-memory WebSocket relay. Configure `[collab].relay_url` to its `ws://` or `wss://` origin. Hosts share a link containing a random room key in the URL fragment; browsers and guests use AES-256-GCM to open encrypted frames. The relay forwards opaque frame bytes and never receives the key or transcript plaintext.

The static browser viewer lives in `assets/collab-web/` and can be hosted separately over HTTPS. Keep full links private: possession of the fragment key grants access. Guest write/steering permission remains a host-side decision; do not expose an unauthenticated relay to untrusted networks.

Each connection owns its socket, so an idle reader cannot block outbound broadcasts. Binary messages are limited to 8 MiB; each recipient has a bounded 16-frame queue and a 10-second stalled-write deadline. An overloaded recipient is disconnected without disconnecting a healthy broadcasting host. Guest envelope peer IDs are replaced with the assigned sender ID. Host disconnect sends `room-closed` to guests and releases room membership, including on transport errors.

The built-in relay does not terminate TLS. For remote deployment, put it behind a TLS reverse proxy and use `wss://`; apply connection and resource limits there. Plain `ws://` links are restricted to loopback hosts.
