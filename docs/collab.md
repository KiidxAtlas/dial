# Encrypted collaboration

Live collaboration lets other people watch a Dial session as it happens and, if you allow it,
prompt the agent, stop its turn or answer its questions. Everything after the relay handshake is
end-to-end encrypted: the relay only forwards opaque frames.

## Quick start

1. Run a relay: `dial relay --bind 127.0.0.1:7331`. It keeps rooms in memory only.
2. Point the host at it in `~/.dial/config.toml`:

   ```toml
   [collab]
   relay_url = "ws://127.0.0.1:7331"   # wss://… for anything but loopback
   ```

3. In the host session, run `/collab` (read-write link) or `/collab view` (read-only link). Dial
   prints the application link and a browser viewer URL.
4. Guests run `/join <link>` in their own Dial, or open the viewer URL in a browser.

`/collab status` shows the room, `/collab list` lists live hosts on this machine (metadata only,
never links), and `/collab stop` closes the room. Guests leave with `/leave`.

## Settings

| Key | Default | Meaning |
|---|---|---|
| `collab.relay_url` | empty | Relay origin (`ws://`, `wss://`, `http://` or `https://`). Required to host. Plain `ws://` only for loopback hosts. |
| `collab.web_url` | empty | Where the browser viewer page is hosted. Empty uses the page `dial relay` serves at the relay's own origin. Must be `https://` (plain `http://` only for localhost). |
| `collab.allow_guest_steering` | `false` | Queue prompts and aborts from writable guests for the host agent. |
| `collab.allow_guest_answers` | `false` | Offer the host's question prompts to writable guests to answer (see below). |

## Links and roles

Two link forms carry authenticated key material, and neither sends it to the relay. The
application link printed by `/collab` is `host[:port]/r/<roomId>.<key-material>`: Dial removes
the key material before connecting to `wss://host/r/<roomId>`. The browser viewer wraps that
application link in a URL fragment, `https://viewer.example/#<link>`, which is not sent to the
viewer server. View links contain the room encryption key and host verification key; full links
also contain the write token. Keep both forms private.

| Role | Link | Can |
|---|---|---|
| Host | — | Run the agent; decide every tool approval. |
| Writable guest | full link from `/collab` | Read; prompt and abort when `allow_guest_steering` is on; answer questions when `allow_guest_answers` is on. |
| Read-only guest | `/collab view` link | Read only. Prompts, aborts and answers are refused locally and by the host. |
| Browser viewer | viewer URL | Read only. |

## Guests in the terminal

A guest joined from the terminal sees the replicated conversation in its transcript, each entry once. A writable guest's prompts are sent to the host agent (`/collab abort` asks the host to stop its running turn); a read-only guest's prompts are refused, and no guest prompt runs on the guest's local agent. The host bounds guest steering: at most 8 queued prompts totalling 256 KiB, 64 KiB per prompt, with aborts coalesced into one. Excess requests get an explicit error reply.

## Guests answering host questions

With `collab.allow_guest_answers = true`, a question the host's agent asks in the terminal (the
`ask_user` choice prompt) is also sent to every writable guest. The guest's transcript lists the
title, body (at most 16 KiB, head and tail kept) and numbered options; `/collab answer <number>`
answers it. The host's own prompt stays open and usable: whichever answer arrives first wins,
the host transcript names the guest who answered, and the question is withdrawn from every guest.
Choosing an "Other (type your own)" option asks the host to type the text.

Guests are never offered tool approvals, permission grants, plan approval or directory requests:
those remain host-only whatever the settings. Each answer is authenticated like a prompt: its
write proof binds the question id and the chosen option, so a relay cannot change or replay it. A
stale, out-of-range or duplicate answer gets an error reply and changes nothing. Read-only rooms
never offer questions.

## Browser viewer

`dial relay` serves the viewer itself: a plain `GET /` on the relay returns the page from
`assets/collab-web/`, and `/collab` prints `http(s)://<relay>/#<link>` when `collab.web_url` is
empty. The room key stays in the fragment, so the relay still sees only metadata. The page is
served with a Content Security Policy that allows only its own inline script and styles (pinned
by hash) and connections back to the relay, plus `no-referrer` and `no-store`. Other paths return
404; WebSocket upgrades go to the relay as before.

The page can also be hosted separately over HTTPS; set `collab.web_url` to it. The viewer reconnects after transient drops, rebuilds its view from each complete snapshot, and reduces frames one at a time in arrival order. Keep full links private: possession of the key grants read access. Guest write/steering permission remains a host-side decision; do not expose an unauthenticated relay to untrusted networks.

Link and connection errors appear in the viewer's status line using the same error style as encrypted shares. Reconnecting is a neutral status, not a terminal error.

## RPC

Native RPC clients use `collaboration_start`, `collaboration_stop`, `collaboration_invite`,
`collaboration_join`, `collaboration_leave` and `collaboration_status`. A joined guest's
`collaboration_status` includes `room.question` (`id`, `title`, `body`, `options`) while the host
has a question open, and `collaboration_answer {option}` answers it with a 1-based option number.
See [`rpc-schema.json`](rpc-schema.json) for parameters and coded refusals.

## Protocol and security

The relay encrypts nothing itself: hosts share a link containing a random room key, and browsers and guests use AES-256-GCM to open encrypted frames. The relay forwards opaque frame bytes and never receives the key or transcript plaintext.

The protocol is version **1004**. Host frames are signed and verified against the host key in
the link. Writable guests prove possession of the write token using a fresh host challenge and
authenticate each mutation; the raw token is never sent in `hello` or other wire frames. A
view-link holder cannot authenticate host frames or gain write authority by claiming the host
role at the relay. Older unsigned links fail closed: regenerate links and update terminal and
browser clients together. Transport role, epoch and sequence replay checks remain in force.
The browser viewer and terminal guest both ignore log entries without an `entry` payload; such a frame does not consume its log index or hide a later valid entry.
Guest answers add the sealed frames `question` and `question-closed` (host to writable guests) and
`answer` (guest to host); they need no protocol version change because older peers ignore them.

**Relay trust assumption:** the relay is trusted for availability, not for integrity. It can drop, delay or close rooms, and it sees metadata such as room IDs, peer IDs, frame sizes and timing. It is not trusted to deliver authentic or fresh frames: a captured frame that the relay redelivers is rejected by every receiver that already processed it, and a host→guest frame cannot be reflected back as a guest command. Replay state lives as long as the local socket owner (a host's state survives reconnects and room recreation, and a restarted host also has a fresh room key). A restarted guest starts with empty replay state, so it can display history it already showed; host→guest frames only update the display, and guest→host commands cannot be replayed into a host that has not restarted.

## Limits

Snapshots for late joiners share the host's log rather than copying it. A peer has at most one unfinished snapshot, a new hello supersedes it, and queued snapshots reference at most 64 MiB of distinct history.

Each connection owns its socket, so an idle reader cannot block outbound broadcasts. Binary messages are limited to 8 MiB; each recipient has a bounded 16-frame queue and a 10-second stalled-write deadline. An overloaded recipient is disconnected without disconnecting a healthy broadcasting host. Guest envelope peer IDs are replaced with the assigned sender ID. Host disconnect sends `room-closed` to guests and releases room membership, including on transport errors.

The relay admits at most 64 connections (including incomplete handshakes and viewer page
requests) and 16 rooms. A request head must arrive within 10 seconds and 8 KiB.
Connection-worker startup failures are logged without ending the listening service. A
non-loopback bind warns about exposure; these limits do not replace a reverse proxy's admission
controls or make an unauthenticated public relay safe against availability attacks.

## Deployment

The built-in relay does not terminate TLS. For remote deployment, put it behind a TLS reverse proxy and use `wss://`; apply connection and resource limits there. Plain `ws://` links are restricted to loopback hosts. Behind a proxy, the relay's viewer page is served at the same `https://` origin.
