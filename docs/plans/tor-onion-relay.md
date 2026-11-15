# Plan: Browser play over Tor through an onion relay on the router

Status: draft v3. Review findings (Fable 5.2) incorporated and Phase 0 done (step 6 optional); see
"Review log" at the end.
Branch: `feat/tor-onion-relay` (from `master`)

## Goal

Let three people play p2skat in **Tor Browser**. The page and a small message
relay are served from **one onion service** running on the OpenWrt
router. Nobody learns anyone else's IP address, and the router needs no port
forward.

Players currently using normal browsers with WebRTC must keep working
unchanged.

## Dependency: the browser frontend is not on `master`

`master` (040826d) has no `web/` or `scripts/` directory and no browser
session in `src/wasm.rs`. That code is on `feat/browser-webrtc` (head 69aa069).
Before Phase 1:

- merge `feat/browser-webrtc` into `master`, then rebase this branch onto it
  (preferred); or
- rebase this branch onto `feat/browser-webrtc` and merge both together.

All `file:line` references below are to `feat/browser-webrtc` at 69aa069.

## Why WebRTC doesn't work here

- Tor Browser turns WebRTC off (`RTCPeerConnection` is undefined) because ICE
  leaks real IPs. Tor carries TCP only.
- A web page can't accept incoming connections or host an onion service.

So every byte goes through a server. The design is a **logical three-way mesh
over a star**: the router relays opaque, end-to-end encrypted blobs between
seats, and the game layer (`MeshReadiness`, `game` packets, the Rust
`Session`) sees the same pairwise channels as today.

## What already exists and gets reused

| Piece | Where | Reuse |
|---|---|---|
| Room lobby protocol (create/join, SSE events, signal, leave) | `scripts/serve.py`, `web/rooms.mjs` | Same endpoints and event shapes, extended with relay frames |
| Duck-typed channels in `PeerMesh` | `web/transport.mjs:203-207, 272-312` (`LocalDataChannel`) | A `RelayChannel` plugs in the same way. `connection.sctp?.maxMessageSize` is undefined-safe (`:317`). `prepare()` skips non-WebRTC peers (`:231`). |
| Worker / Wasm boundary | `web/worker.js`, `src/wasm.rs` | Unchanged. The transport is invisible to Rust. |
| SRA fairness | `src/crypto` | Unchanged |

## Architecture

```
Tor Browser ×3 ──► Tor ──► router (OpenWrt 24.10, aarch64)
                            ├─ tor (opkg): HiddenServiceDir → 127.0.0.1:8080
                            └─ p2skat-room (static Rust binary)
                                 ├─ GET  /…                       static web/ + web/pkg/
                                 ├─ POST /api/rooms               create (mode: webrtc|relay)
                                 ├─ POST /api/rooms/{id}/join
                                 ├─ GET  /api/rooms/{id}/events?token=&after=   (SSE)
                                 ├─ POST /api/rooms/{id}/signal   (webrtc rooms only)
                                 ├─ POST /api/rooms/{id}/relay    (relay rooms only, batched)
                                 ├─ POST /api/rooms/{id}/leave
                                 └─ GET  /api/health
```

One origin, one onion service. The page loads nothing from any other origin.

## Phases

The order is chosen to retire risk early. The relay protocol and client are
**prototyped in `serve.py` first** (it's small) and proven over a laptop onion
service. Only then is the server ported to Rust and deployed on the router.

### Phase 0: feasibility spike (1 day, do this first)

On a laptop: `tor` with a throwaway `HiddenServiceDir` pointing at
`python3 scripts/serve.py`, opened in current Tor Browser.

1. `isSecureContext` is true on `http://….onion`. `crypto.subtle` (ECDH P-256,
   HKDF, HMAC, AES-GCM) and `crypto.randomUUID` work.
2. **Origin header** on a same-origin `fetch` POST from the onion page: is it
   the onion origin or `null`? `serve.py:125-130` rejects anything that doesn't
   match `Host`, which would 403 every API call. Adjust the rule if needed.
3. Wasm and module `Worker` load at **Standard**. At **Safer**, detect real
   availability by *instantiating* a tiny module: NoScript may block Wasm
   while the `WebAssembly` object still exists. At **Safest** (no JS), a
   `<noscript>` message must explain the problem.
4. `EventSource` and `fetch` POST latency over the onion for 1 KB messages.
   Check that `EventSource` reconnect works after the server drops a stream.
5. **WebSocket alternative**: check `ws://….onion` works from the page. If it
   does, compare it against batched POST + SSE (see Phase 1 "Transport
   choice").
6. **Real deal time**: hack a `/relay` passthrough into `serve.py` (~40 lines,
   plaintext, throwaway) and a minimal `RelayChannel`, then time one full deal
   across three Tor Browsers. A deal is 15 sequential message hops
   (3 shuffle passes, 12 deal steps; see `src/session.rs:280-420`), so
   expect ~9–17 s from the measured per-hop latency. **Optional now**: the
   per-hop latency is measured and the 5-hop deal (Related work) is the fix
   if this is too slow. It can fold into the Phase 4 `--relay` smoke run
   instead of a throwaway prototype.
7. **Router**: cross-build a hello-world for `aarch64-unknown-linux-musl` and
   run it on the router. Record free RAM and overlay space, and confirm the
   package manager (`opkg` expected on 24.10). **Done, see results below.**

Record all results in this document before Phase 1.

#### Phase 0 results

**Step 7, router, 2026-11-15: passed.**

| Check | Result |
|---|---|
| Release / target | OpenWrt 24.10, `aarch64_cortex-a53` |
| RAM | Several hundred MB available, no swap. Idle load ≈ 0. |
| Flash | Ample free space on `/overlay`, tmpfs on `/tmp`. |
| Package manager | `opkg` (not `apk`) |
| Tor | `tor` 0.4.9.13-r1 in the feed (1.4 MB). Deps: libevent2, libopenssl3, zlib, libcap. Also `tor-basic` (no relay/bridge code; enough for an onion service) and `tor-hs` (a UCI configurator for onion services, worth a look for Phase 5). |
| NTP | `system.ntp.enabled = 1` |
| Existing web server | `uhttpd` (LuCI) on the LAN. No conflict: `p2skat-room` binds `127.0.0.1:8080`. |
| Cross-build | Rust 1.94 (repo's pinned rust-overlay) with target `aarch64-unknown-linux-musl`, `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld`. **No extra linker-flavor or self-contained flags needed.** Build scripts still need the host `cc`, so build inside `nix develop`. |
| Test binary | tokio (current-thread) + hyper 1 http1 + serde_json, `[profile.router]` as specified: **594 KB** static, stripped. Ran from `/tmp`, served 201 requests, **VmHWM 556 KB**. |

Consequences for the plan:

- The < 3 MB / < 20 MB RSS targets have plenty of headroom. RAM isn't a
  constraint: keep `--max-rooms 8` as a sanity cap, not a memory budget.
- Prefer `tor-basic` over `tor` in Phase 5.

**Steps 1–5, Tor Browser, 2026-11-15: passed. Step 6 is still open.**

Setup: Tor Browser 15.0.25 (Firefox 140 ESR, Linux) → throwaway onion service
on a laptop (tor 0.4.9.15). A probe page was served with the CSP the plan
proposes, next to `serve.py` and the `feat/browser-webrtc` build.

| Check | Standard | Safer |
|---|---|---|
| `isSecureContext` on `http://….onion` | ✅ true | ✅ true |
| `RTCPeerConnection` | absent | absent |
| `crypto.randomUUID`, `crypto.subtle` | ✅ | ✅ |
| ECDH P-256 → HKDF → AES-GCM (with AAD) round trip | ✅ | ✅ |
| HMAC-SHA256 via HKDF, sign + verify (tamper detected) | ✅ | ✅ |
| X25519 in WebCrypto | supported | supported |
| `WebAssembly` | ✅ instantiates under the CSP | ❌ **`WebAssembly` is undefined** (page and workers) |
| Module worker + p2skat `crypto_self_check` | ✅ (also via the real app's self-check) | ❌ no Wasm |
| Real app, Create table | ❌ fails at `networkConfiguration()` (no `RTCPeerConnection`), as expected | — |

Security level doesn't affect the network results:

| Check | Result |
|---|---|
| `Origin` on same-origin POST | `http://<addr>.onion:8081`, exactly matching `Host`. `serve.py`'s origin rule accepts it, so no change needed. |
| POST round trip, 1 KB, sequential ×15 | min 417, **median 600**, p90 850–1317, max 1400 ms |
| POST → SSE delivery (= one relay hop) ×10 | min 383, **median 567**, p90 1117 ms |
| SSE automatic reconnect | ✅ sends `Last-Event-ID` (got `3` after a server-side drop). `readyState` stays `CONNECTING` (0) during the retry, not `CLOSED`. |
| WebSocket `ws://` same-origin echo, 1 KB ×10 | ✅ works. min 350, **median 550**, p90 800 ms |

Consequences for the plan:

- **Phase 2 crypto works as designed** (P-256, HKDF, HMAC, AES-GCM) in Tor
  Browser. Keep P-256: X25519 also works here, but P-256 has no ESR-version
  risk.
- **Standard level is required.** Detection is simply `typeof WebAssembly
  === 'undefined'`. The page doesn't need to instantiate a test module (review
  item I7 resolved). Safest (no JS) still needs the `<noscript>` message.
- **The Origin check stays as is** (review item I7 resolved).
- **Transport decision: batched POST + SSE.** WebSocket works with the same
  latency (550 vs 567 ms per hop), so it buys no speed. SSE's built-in
  `Last-Event-ID` reconnect is exactly the resume mechanism Phase 1 needs, the
  lobby already uses POST + SSE, and `serve.py` stays stdlib-only for the
  prototype. Revisit only if step 6 shows HTTP overhead matters.
- **Deal time estimate**: about 15 sequential relay hops × 0.57–1.1 s ≈
  **9–17 s per deal** plus crypto. This is consistent with the 10–30 s
  planning figure. Step 6 measures it for real.

### Phase 1: relay protocol (prototype in `serve.py`, then the Rust server)

#### Transport choice

**Decided (Phase 0): batched POST upstream + SSE downstream.** WebSocket over
onion works with the same latency, so it buys nothing. SSE's native
`Last-Event-ID` reconnect, confirmed in Tor Browser, provides resume, and the
lobby already uses this machinery.

#### Relay frames

- `POST /api/rooms/{id}/relay` with a bearer token and body
  `{ "frames": [ { "to": seat, "seq": u64, "payload": base64url } ] }`.
- The client has **one POST in flight at a time** and batches everything
  queued meanwhile into the next POST. Parallel POSTs would travel on separate
  HTTP/1.1 connections, which means separate Tor streams with no ordering
  guarantee. Onion is HTTP/1.1 only and allows 6 connections per host, one of
  them held by SSE. The game is mostly ping-pong (`session.rs:337-412`), so
  batching costs nothing.
- Server per (sender, to) pair: frames with `seq <= last` are already
  delivered and silently skipped. `seq == last+1` is forwarded. A gap rejects
  the batch. The response is **always** `200 { "expected": { "<to>": n, … } }`
  for a valid batch, or a typed error body `{ "error": "...", "code": "gap" |
  "room_closed" | "over_limit" | ... }`. Error meanings are not overloaded onto
  409.
- A retried POST resends **identical ciphertext**. The client never
  re-encrypts a frame under the same `seq`.
- Payload cap: 64 KiB + 64 bytes decoded. The plaintext is already capped at
  64 KiB by `checkText` (`transport.mjs:318`). The client-side cap on SSE
  events (`parseRoomEvent`, `rooms.mjs:15`, 128 KiB) still has headroom for one
  base64 frame plus envelope. Forward one frame per SSE event.
- Forwarded as SSE `id: <n>` + `data: {"type":"relay","from":seat,"seq":n,"payload":"…"}`.
  The server sets `from`. The client never supplies it.
- `POST` timeouts: the client puts an `AbortController` on every POST (30 s)
  and retries with backoff. That's safe because of `seq` deduplication.

#### Event streams that survive Tor hiccups

Today a second stream for the same player returns 409
(`serve.py:270-271`), and the client tears everything down on the first SSE
error (`rooms.mjs:81-84`). Over Tor, both have to change:

- **A new stream supersedes the old one.** The server closes the previous
  stream for that player and attaches the new one. It does not 409. After a
  silent circuit death the server can't tell that the old stream is dead.
- **Resume by event id.** Every event has a per-player monotonically
  increasing `id:`. The server keeps a per-player **acked log**: events stay
  until the player reconnects with `after=<id>` (query parameter, plus the
  `Last-Event-ID` header that `EventSource`'s automatic reconnect sends) or
  until limits are hit. The log is the delivery queue; there's no separate
  ring buffer.
- **Watchdog.** `EventSource` never surfaces `:` comment lines, so the
  heartbeat becomes a named event (`event: ping`, every 10 s). If the client
  sees no event for 25 s, it closes the `EventSource` and reconnects by hand
  with `?after=<last id>`. The same path handles `EventSource` giving up after
  a non-200. Send `retry: 2000`.
- **Peer liveness, server side.** If a player has had no attached stream for
  120 s, the server closes the room with "<name> lost connection". This is the
  only reliable "peer vanished" signal (`pagehide` → leave at `app.js:108` is
  unreliable on Android). In relay mode, room-stream errors after the game
  starts are **fatal** at the client. Today they're ignored once
  `mesh.started` (`app.js:223-225`).

#### Memory limits (router-sized)

- Per player: acked log ≤ 256 events and ≤ 1 MiB.
- Global: ≤ 16 MiB across all logs. `MAX_ROOMS` is configurable; the default
  is 64 for dev and the router deploys with 8.
- A token bucket of 50 frames/s per player. Going over any limit closes the
  room with a clear message.
- Rooms live in memory. A server restart ends all games. The README says so.

#### Room mode

- `POST /api/rooms` takes `mode: "webrtc" | "relay"`. It's fixed for the
  room's lifetime. `signal` is rejected in relay rooms and `relay` in WebRTC
  rooms.
- The client does **not** trust the server's mode. In relay rooms the invite
  fragment carries `m=relay` (Phase 2), which is bound into the key-frame MAC.
  A client that intends relay mode fails closed if the server says otherwise.
  That stops a malicious server from downgrading a privacy-minded clearnet
  player to WebRTC to learn their IP.

#### Rust server `p2skat-room`

- New crate `room/` in a Cargo workspace. It does not depend on the main crate
  (no arti, ratatui, num-bigint). Root manifest gets `[workspace]`,
  `members = [".", "room"]`, `default-members = ["."]` so `packages.default` in
  `flake.nix` and `build-web.sh --locked` are unaffected.
- Dependencies: `tokio` (rt, net, time, sync, macros), `hyper` 1 +
  `hyper-util`, `serde`, `serde_json`, `getrandom`. Nothing else.
- Same routes and JSON as `serve.py` plus the changes above.
- Static files from `--web-dir`: correct MIME types (`application/wasm`,
  `text/javascript`), path traversal blocked. Headers:
  `Cache-Control: no-store` on `/api/`, `X-Content-Type-Options: nosniff`,
  `Referrer-Policy: no-referrer`, and the CSP
  `default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; connect-src 'self'; img-src 'self' data:; style-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'`.
  `index.html` has no inline scripts or styles.
- Origin rule as settled in Phase 0. `--allowed-host xyz.onion` limits the
  accepted `Host` when deployed.
- Binds `127.0.0.1:8080` by default. Never logs tokens, room IDs or payloads.
  `--log` gives counts only.
- `[profile.router]` in the **root** manifest: `opt-level="z"`, `lto`,
  `panic="abort"`, `codegen-units=1`, `strip`.
- Add `aarch64-unknown-linux-musl` to the flake's rust targets. Link with
  `rust-lld` (`CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld`,
  verified in Phase 0). `scripts/build-router.sh` runs inside `nix develop` and
  produces the binary. Target: < 3 MB (a hyper spike was 594 KB).
- Tests: make `scripts/test_serve.py` a **conformance suite parameterized by
  server URL**. It's black-box HTTP apart from its `from serve import …`. Run
  it against both servers until `serve.py` is retired. Add relay tests to it:
  dedup, gap, batch, limits, mode mismatch, stream supersede, resume with
  `after`, room closes after 120 s with no stream.
- When the Rust server passes the suite, the README switches to it, and
  `serve.py` is removed in a follow-up (the suite stays).

#### Phase 1 results: the protocol as implemented (2026-11-15)

Both `scripts/serve.py` and `room/` (`p2skat-room`) implement this; the
29-test conformance suite in `scripts/test_serve.py` passes against both.
The aarch64 router binary is 774 KB.

**Errors.** Every API error is `{"error": "<human text>", "code": "<code>"}`.
Clients branch on `code`, never on the status alone:

| Status | `code` | When |
|---|---|---|
| 400 | `bad_request` | malformed body, frame, seq, payload, `ack`, `after` |
| 400 | `wrong_mode` | `signal` in a relay room, `relay` in a WebRTC room, `join` with the other `mode` |
| 401 | `unauthorized` | bad token |
| 403 | `forbidden` | Origin rule |
| 404 | `not_found` / `room_closed` | unknown table or endpoint / table closed (kept 30 s) |
| 409 | `room_full` / `duplicate_signal` | |
| 413 | `too_large` | body or frame payload over the cap |
| 415 | `unsupported_media_type` | not JSON |
| 421 | `wrong_host` | `--allowed-host` mismatch |
| 422 | `gap` | also carries `"expected"` (below); nothing in the batch was applied |
| 429 | `over_limit` | rate or memory limit; the table is now **closed** with the same message |
| 503 | `busy` | `--max-rooms` reached |

**Create / join.** `POST /api/rooms` `{"name", "browser", "mode"?}` (`mode`
defaults to `"webrtc"`). `POST /api/rooms/{id}/join` `{"name", "browser",
"mode"?}`; a `mode` that differs from the table's fails with `wrong_mode`
without taking a seat. Both return
`{"room", "seat", "token", "players": [{"seat", "name", "browser"}], "mode"}`.

**Relay.** `POST /api/rooms/{id}/relay`, `Authorization: Bearer <token>`,
body `{"frames": [{"to": seat, "seq": n, "payload": "<base64url>"}], "ack"?: id}`.

- Exactly those keys; `seq` is 1 … 2^53−1, per (sender, `to`); the first frame
  on a pair is `seq` 1.
- `payload` is unpadded base64url, non-empty, ≤ 65 600 decoded bytes (413
  above). Body ≤ 1 MiB + 4 KiB.
- `seq <= last` is skipped, `last+1` is forwarded, anything larger rejects the
  whole batch with 422 `gap`.
- `frames: []` is valid (an ack-only POST). `ack` (optional) discards the
  sender's own log entries with id ≤ `ack`; it must be < the next event id.
  **The browser client should send `ack: <last event id>` on every relay POST**
  so the log stays small during a healthy stream.
- 200 response: `{"expected": {"<seat>": next_seq, …}}` for every other seated
  player (e.g. `{"1": 4, "2": 1}`). The 422 body carries the same `expected`.
- Token bucket: 50 frames/s, burst 50, counted per frame (duplicates
  included).

**Events.** `GET /api/rooms/{id}/events?token=<token>&after=<id>`. The resume
point is `max(after, Last-Event-ID)`; an id beyond the last event is 400.
Connecting acknowledges (discards) every event ≤ that point, supersedes any
older stream for the player (it ends normally), and replays the rest. Stream
format:

```
retry: 2000

data: {"type":"players","players":[…]}          ← no id: snapshot on connect and on every join, not logged

id: 7
data: {"type":"relay","from":0,"seq":3,"payload":"…"}

id: 2
data: {"type":"signal","from":0,"signal":"…"}   ← WebRTC rooms only

event: ping
data: {"type":"ping"}                            ← after 10 s without other output, no id

id: 8
data: {"type":"closed","message":"Bert lost connection."}   ← last event; the stream then ends
```

Event ids are per player, start at 1 and increase by one per logged event
(`relay`, `signal`, `closed`). `from` is always set by the server. A stream
opened on a recently closed table replays only its `closed` event.

**Log and limits.** Each player's log holds events until acknowledged. When a
limit (256 events or 1 MiB per player, 16 MiB total) is reached, events that
were already written to the player's stream are discarded first; if that is
not enough the table closes with `over_limit`. A later resume from before a
discarded, unacknowledged event closes the table with "<name> missed messages
after a connection problem."

**Liveness.** Relay tables only: a player without a stream for 120 s closes
the table with "<name> lost connection." WebRTC tables keep today's behaviour
(signaling loss doesn't end a peer game). Tables expire after 1 h without
activity; an attached stream counts as activity.

**Knobs (environment, both servers).** `P2SKAT_HEARTBEAT_MS` (10000),
`P2SKAT_LIVENESS_MS` (120000), `P2SKAT_ROOM_TTL_MS` (3600000),
`P2SKAT_CLOSED_RETAIN_MS` (30000), `P2SKAT_RATE` / `P2SKAT_BURST` (50),
`P2SKAT_LOG_EVENTS` (256), `P2SKAT_LOG_BYTES` (1048576),
`P2SKAT_TOTAL_BYTES` (16777216). Fault injection, tests only:
`P2SKAT_FAULT_LATENCY_MS=800,400` delays every logged SSE event and every POST
response by 800 ± 400 ms (order per player is kept);
`P2SKAT_FAULT_DROP_S=N` cuts one random attached player's SSE stream and
in-flight (delayed) POSTs every N s. A dropped POST may already have been
applied: retry it unchanged.

### Phase 2: end-to-end encrypted relay channels (browser)

**What this buys, stated honestly.** Much game traffic is broadcast anyway
(`session.rs:319, 455, 579, 601, 726, 763`). The pairwise messages
(`:289, 303, 353, 365, 412`) are SRA ciphertexts designed for public channels.
The server stamps `from`, so a third player can't inject frames without the
server's help. What the layer actually provides:

- confidentiality and integrity **against the relay host** (and anyone who can
  read router memory);
- **forward secrecy** if an invite link leaks after the game (ephemeral ECDH);
- detection of server-side tampering, reordering, dropping (as a gap) and mode
  downgrade.

Out of scope: anyone holding the invite link *and* controlling the server can
MITM fully. This is the same trust model as WebRTC signaling today.

**Invite fragment**: `#room=<id>&k=<secret>&m=relay`, where `secret` is 32
random bytes in base64url. The fragment is never sent to any server. A relay
room invite without `k` shows a clear error ("this invite link is incomplete").
Today `app.js:185, 208` reads only `room`.

**Handshake** (per pair, over the relay, before `hello`). Everything uses
WebCrypto:

1. Each seat makes one ephemeral, non-extractable ECDH P-256 key pair per room.
2. `auth_key`: `importKey('raw', secret, 'HKDF')` → `deriveKey` to HMAC-SHA256
   with `salt="p2skat-relay-v1"`, `info="auth|"+room_id`.
3. A `key` frame to each other seat:
   `{type:"key", from, to, room_id, mode, names, pub}` with
   `mac = HMAC(auth_key, canonical(...))`. `canonical` is a
   **length-prefixed** encoding, because names are arbitrary Unicode.
   Key frames are not AEAD-encrypted. They use `seq` 1 on the pair.
4. The receiver checks with `crypto.subtle.verify` (constant time), then checks
   the seats, names and `mode == "relay"`. Any mismatch fails with "this invite
   link was tampered with or is for another table".
5. `deriveBits(ECDH)` → `importKey('raw', …, 'HKDF')` → two `deriveKey` calls
   to AES-GCM-256, one per direction. `salt = secret`,
   `info = "p2skat-relay-v1|"+room_id+"|"+lo_seat+"|"+hi_seat+"|"+dir+"|"+pub_lo+"|"+pub_hi`.

**Frames**: AES-GCM, nonce = 4-byte direction tag ‖ 8-byte big-endian `seq`.
Keys are per direction, so the tag is defence in depth. AAD =
`room_id|from|to|seq`. The plaintext is exactly the existing `PeerMesh` packet
JSON (`hello`, `ready`, `game`, `bye`).

**Receive rule**: `seq <= last` → drop silently (resume and retries are
at-least-once). `seq == last+1` → decrypt, and an AEAD failure is fatal.
`seq > last+1` → fatal gap. Server ordering is checked, not trusted.

**`web/relay.mjs`**: `RelayChannel` with the duck-typed surface `attach()`
uses (`label`, `ordered`, `maxRetransmits`, `maxPacketLifeTime`, `readyState`,
`bufferedAmount`, `send`, `close`, `on*`).

- `send()` is synchronous, like `RTCDataChannel.send`. It **assigns `seq`
  before any `await`**, then appends to the pair's encrypt chain (a promise
  chain, so the order stays fixed). It throws if over the buffered-bytes
  limit.
- A shared uploader drains all pairs' encrypted frames into one in-flight
  batched POST.
- `close()`: `PeerMesh.close()` sends `bye` and closes immediately
  (`transport.mjs:336-345`), so a `bye` may be lost. That's acceptable: peers
  learn about it from the server's `closed` event after `leave`
  (`app.js:217`), or from the 120 s server liveness timeout.

### Phase 3: wire it into the app

- `web/transport.mjs`: add a `relayPeers` branch next to `localPeers`
  (`:203-207`). No general `channelFactory`. The existing tests must pass
  unchanged.
- `web/rooms.mjs`: `enter({ mode })`, `relay` and `ping` events, event-id
  tracking, the watchdog and manual reconnect with `?after=`, and a
  `sendFrames()` uploader.
- `web/app.js`:
  - **Choose the mode before `networkConfiguration()`**. That function throws
    without `RTCPeerConnection` (`app.js:110-116`) and runs before entering the
    room (`:200`).
  - If there's no `RTCPeerConnection`, or the page is on a `.onion` host, or the
    invite has `m=relay`: relay mode. Hide the STUN/TURN fields and say why.
  - Otherwise use WebRTC as today. An advanced checkbox, "Route game through
    the table server (no direct connections)", creates a relay room. It's for
    testing in normal browsers and for people who don't want to share IPs.
  - The `BroadcastChannel` same-browser shortcut stays WebRTC-mode only.
  - Room-stream errors are fatal after start in relay mode (Phase 1).
- Capability check on load: secure context, `crypto.subtle`,
  `typeof WebAssembly !== 'undefined'` (at Safer it's simply undefined;
  Phase 0). Plus a `<noscript>`. Message: "In Tor Browser, set Security
  Level to Standard for this site."
- Document in the UI that **reloading the page, or Android suspending the tab
  for long, ends the game**. Keys live only in memory and there's no rejoin.
- Progress text for slow phases: "Exchanging keys…", "Shuffling over Tor, this
  can take half a minute…".

### Phase 4: tests

- `node --test` units for `relay.mjs` with fake fetch/SSE: tampered MAC, wrong
  names, mode mismatch, duplicate frames dropped, gap fatal, AEAD failure
  fatal, third party can't decrypt a pair's frames, `send()` ordering under
  async encryption, batching.
- Conformance suite (Phase 1) against both servers.
- `scripts/browser-smoke.cjs --relay`: three Chromium pages, relay mode forced,
  `RTCPeerConnection` deleted in an init script to mimic Tor Browser, two full
  rounds.
- **Fault injection** flags on the server (test builds / env var only):
  - latency: delay every SSE event and POST response by 800 ± 400 ms;
  - drop: kill a random player's SSE and in-flight POST connections every N
    seconds.

  The `--relay` smoke run must finish two rounds under both, with a raised
  deadline. Record deal time.
- Manual: three real Tor Browsers on separate devices against the
  router.

#### Phase 2–4 results (2026-11-15)

Implemented as specified in `web/relay.mjs` (`RelaySession`, `RelayChannel`,
`RelayUploader`), `web/rooms.mjs`, `web/transport.mjs` (`relayPeers`) and
`web/app.js`. Choices the plan left open:

- **Key frame encoding.** The payload is base64url JSON
  `{type, from, to, room_id, mode, names, pub, mac}` (`pub` = raw uncompressed
  P-256 point, `mac` = HMAC-SHA256, both base64url). The MAC input is
  `canonical("p2skat-relay-v1", "key", from, to, room_id, mode, "3", name0,
  name1, name2, pub)`, each part a 4-byte big-endian length plus UTF-8 (or raw)
  bytes. Structure errors, a bad MAC and any mismatch of `from` (server-stamped),
  `to`, room, names or `mode != "relay"` all fail with the same message.
- **Direction keys.** `dir` in the HKDF info is `"<from>><to>"`; `pub_lo` /
  `pub_hi` are the base64url public keys. The nonce tag is the bytes
  `0x70 0x32 from to`.
- **Uploader.** Frames stay queued until a 200 `expected` confirms them; a 200
  that doesn't confirm a frame it carried is fatal. A 422 `gap` skips the frames
  the server says it has and resends from there; if that doesn't line up with
  the queue (the server lost a confirmed frame) it's fatal. Network errors,
  aborts (30 s) and 5xx retry with backoff (250 ms … 8 s) for up to 120 s since
  the last success; 404 `room_closed` waits 3 s for the stream's `closed`
  reason; every other error ends the table with the server's text. Batches
  hold ≤ 32 frames. An idle uploader **lingers 15 ms** (not in hidden tabs) so
  a burst such as the deck to both peers shares one POST: under the latency
  fault this cut POSTs per seat from ~60 to ~37 and a round from ~64–70 s to
  ~47–57 s. An ack-only POST goes out after 32 unacknowledged events.
- **Stream.** Relay rooms only: logged events at or below the last id are
  dropped (the `lastEventId` buffer persists across id-less `players`/`ping`
  events, so only `relay`/`closed` are checked). A 1 s timer reopens the stream
  with `?after=` after 25 s without any event; an `EventSource` that reached
  `CLOSED` is reopened after 2 s; after 150 s without a working stream the
  table ends. WebRTC rooms keep the old one-shot behaviour.
- `bye` is never delivered in relay mode (the session closes before it's
  encrypted). Peers learn from `closed` after `leave`, or from liveness.

Tests: 25 `node --test` cases in `web/relay.test.mjs` (plus 4 relay cases in
`web/rooms.test.mjs`); 47 in total pass. Acceptance 4 is checked in the
`--relay` smoke run: the page wraps `fetch` and `EventSource`, and every
payload any seat posted or received (≈ 200 per run) must be one of the 6 key
frames or non-JSON ciphertext without packet markers.

`browser-smoke.cjs --relay`, headless Chromium, three isolated profiles,
`RTCPeerConnection` deleted, two full rounds, then one tab closes with its
`leave` blocked (`P2SKAT_LIVENESS_MS` 8000 or 10000). Deal = `round` →
`hand` event, slowest seat; the crypto alone is ≈ 3 s here.

| Server, faults | Deal (r1 / r2) | Round (r1 / r2) | Tab closed → others told |
|---|---|---|---|
| Rust | 3.3 / 3.3 s | 14.0 / 13.2 s | 8.1 s |
| serve.py | 3.3 / 3.3 s | 14.0 / 13.3 s | 20.8 s |
| Rust, latency 800±400 | 13.7 / 14.2 s | 56.8 / 53.0 s | 11.0 s |
| serve.py, latency 800±400 | 13.8 / 13.2 s | 55.2 / 46.8 s | 26.2 s |
| Rust, drop every 15 s | 3.0 / 3.1 s | 9.4 / 8.5 s | 10.0 s |
| serve.py, drop every 15 s | 3.0 / 3.1 s | 9.3 / 8.7 s | 12.9 s |
| Rust, latency + drop every 8 s | 8.9 / 8.6 s | 49.1 / 42.0 s | 11.3 s |
| WebRTC (`TEST_WEBRTC=1`), for comparison | 3.1 / 3.4 s | 14.8 / 12.2 s | — |

Notes:

- Under latency the mesh was ready ~4–5 s after the last join (key frame,
  `hello`, `ready`: three hops).
- Drops cut 1–12 streams per run; `EventSource`'s own `Last-Event-ID`
  reconnect resumed every one, so the manual reconnect path ran only in unit
  tests. No relay POST ever failed at the page: Chromium transparently resends
  a POST whose reused connection closes before the response. The retry path is
  covered by unit tests.
- serve.py notices a vanished stream only at its next heartbeat write, so its
  liveness timeout fires up to 10 s late. Within the plan's "within 120 s".
- A deal under injected latency (~14 s) is mostly the hop chain plus the one
  POST in flight: seat 2 sends four messages during hop 3, and the later ones
  wait for the earlier POST's (delayed) response. Real Tor should land between
  this and the 3 s local figure.

### Phase 5: deployment on the router

Target: OpenWrt 24.10 (`opkg`), `aarch64_cortex-a53`.

1. `opkg install tor-basic` (or use `tor-hs` for UCI-managed config).
   `/etc/tor/torrc`:
   ```
   SocksPort 0
   HiddenServiceDir /etc/tor/p2skat/
   HiddenServicePort 80 127.0.0.1:8080
   ```
   No `HiddenServiceSingleHopMode`. `HiddenServiceDir` must be owned by the
   `tor` user, with mode 0700.
2. Create an unprivileged `p2skat` user (it must be in `/etc/passwd` for
   procd's `user`). Binary in `/usr/bin/p2skat-room`, files in
   `/usr/share/p2skat/web/`, and a procd init script at
   `/etc/init.d/p2skat-room` (`respawn`,
   `--bind 127.0.0.1:8080 --web-dir … --allowed-host <addr>.onion --max-rooms 8`).
3. Check NTP is enabled; Tor refuses to bootstrap with a wrong clock.
4. **Sysupgrade**: packages and `/usr/{bin,share}` don't survive. Add
   `/etc/tor/` and the init script to `/etc/sysupgrade.conf`. Make
   `scripts/deploy-router.sh <ssh-host>` idempotent: install `tor` if missing,
   create the user, copy files, enable and restart the services, print the
   onion address. After an upgrade, re-running it is the documented restore
   path. Back up `/etc/tor/p2skat/` offline: it's the table's identity.
5. Keep no hostnames or addresses in the repo.
6. README section "Play over Tor" with the caveats below.

#### Phase 5 status (2026-11-15): tooling written, not yet run on the router

- `scripts/deploy-router.sh <ssh-host>` (local, bash) builds or reuses the
  router binary and `web/pkg`, stages only the served files (html/css/js/mjs,
  `pkg/*.js`, `pkg/*.wasm`; ≈ 680 KiB gzipped), copies them with `scp -O` and
  runs `scripts/router-setup.sh install` (POSIX sh, busybox ash) as root. One
  SSH control connection per run; `SSH_CONFIG` passes `-F`. `--dry-run`
  builds and stages, then prints the commands and the generated files.
- Router side, idempotent: `opkg install tor-basic` only if `/usr/sbin/tor`
  is missing; `p2skat` user and group through `/lib/functions.sh`
  (`group_add_next`, `user_add`); binary and web files swapped in only if
  their checksums changed; a marked block in the torrc that tor reads
  (`uci get tor.conf.default`, default `/etc/tor/torrc`). `SocksPort 0` is
  added only if torrc has no SocksPort of its own. `/etc/tor/p2skat` belongs
  to tor's account: torrc `User`, else procd's `user` in `/etc/init.d/tor`,
  else root. tor restarts only if the block changed or tor isn't running; the
  script waits ≤ 60 s for `hostname`, writes the address to
  `/etc/config/p2skat` (UCI, read by the procd init script), adds a marked
  `/etc/tor/`, init script and config block to `/etc/sysupgrade.conf`, and
  restarts `p2skat-room` only if anything changed.
- `--allowed-host` also applies to `/api/health`, so the router-side health
  check sends `Host: <addr>.onion`: `wget --header` (busybox or GNU wget),
  else `nc`, else it accepts the 421 a foreign Host gets as a sign of life,
  because OpenWrt's default `wget` (uclient-fetch) can't set headers.
- `--uninstall [--purge-keys]` removes service, files and both marked blocks
  and keeps the keys by default (with a warning). `--check` runs the
  conformance suite through `ssh -O forward`: `StaticTests` against the live
  server (it creates no tables, so it can't hit `--max-rooms 8` or disturb
  games) and the full suite (`P2SKAT_SERVER_FAST=1`) against a temporary
  root-owned instance of the deployed binary with the same `--allowed-host`.
  It reports binary size and VmHWM/VmRSS and judges acceptance 6. The suite
  gained `P2SKAT_SERVER_HOST` for the Host/Origin header.
- `scripts/onion-local.sh [port]` runs a throwaway onion service with the
  system tor.
- Verified locally only: shellcheck, `busybox ash -n`, router-side
  install/re-install/uninstall in a fake root under busybox ash with stub
  `opkg`/`uci`/`rc.common`; `--check` end to end with a fake `ssh` against a
  native server; `onion-local.sh` with `DisableNetwork 1`.
- To confirm on the router: tor-basic's torrc has `User tor` (else the HS
  directory belongs to root, with a warning); `tor --verify-config` as root
  accepts the block; whether `nc`/`setsid` exist in the busybox build; that
  procd honours `no_new_privs`. Then run `--check` and acceptance 1.

## Risks and caveats

- **Weak uplink.** Mitigated by stream supersede, resume, the
  watchdog and the 120 s liveness window. The same binary and torrc run on any
  other always-on box if the router is too flaky.
- **Latency**: 0.57–1.1 s per relay hop (Phase 0). That's ~9–17 s per deal
  with today's 15-hop deal, ~3–6 s with the 5-hop deal. Card play is fine.
- **The host serves the code.** E2E protects against a passive or curious host,
  not one serving modified JS/Wasm. Acceptable among friends; the README says
  so.
- **Metadata**: timing, counts and sizes are visible to the relay. No padding.
- **Reload ends the game**; no rejoin.
- **Onion key = table identity.** Losing it changes the address. Leaking it
  allows impersonation.

## Related work: 5-hop deal (separate change, recommended first)

Today a deal is 15 sequential hops. The shuffle takes 3. Then `deal_phase`
deals the three hands one after another, with owner↔helper round trips:
4 hops × 3 = 12. SRA layers commute and the hands are independent, so the
deal can be restructured to 5 hops: hands in parallel, layers stripped in a
chain helper→helper→owner, and seat 2 starting as soon as it holds the final
deck. Over Tor that's ~3–6 s per deal instead of ~9–17 s, and it helps
WebRTC and native TCP/Tor equally.

It's a wire-protocol change in `Session` shared by every transport, so do it
as its own change, together with the QR-encoding fix below and the
decryption-oracle check (helpers must verify they are decrypting the deck's
own ciphertexts), under one protocol version bump. It isn't a blocker for
this plan: the relay works with either deal, and Phase 4 timings just need
re-measuring after it lands.

**Status: done (protocol v2).** Deal is 5 hops (seats 0 and 1 hold their
hands after hop 4), skat pickup 2 hops after the request, both asserted by
tests in `src/session.rs`. Schedule and oracle argument are in the
`src/session.rs` module docs.

## Separate issue found during review (not part of this plan)

Card plaintexts are raw indices 1..32 (`src/crypto/protocol.rs`
`card_values()`). With SRA, a card's encryption preserves whether it is a
quadratic residue mod p, so every player learns that one bit of each
encrypted card. The standard fix is to map cards to quadratic residues (e.g.
encode card `i` as the i-th QR, or square each value). Track it separately. It
affects all transports.

**Status: done (protocol v2).** Squaring small values would keep SRA's
multiplicative relations (`E(2²)² = E(4²)`), so cards are squares of
hash-derived values mod p (`CardCodec` in `src/crypto/protocol.rs`).

## Out of scope

- Embedding arti in `p2skat-room` instead of system `tor`.
- Static hosting with public signaling (Nostr) for the WebRTC path.
- Rejoin after a reload.
- The 5-hop deal (see "Related work"; tracked separately).
- Tor Browser ↔ native Tor client interop.

## Acceptance criteria

1. Three Tor Browsers at Standard level, on different networks, play two full
   rounds via the router's onion address.
2. `browser-smoke.cjs --relay` passes, including under the latency and drop
   fault injections.
3. The existing WebRTC smoke test and unit tests pass unchanged.
4. A test dumps every payload the relay stored and asserts that none contains
   a plaintext packet (`"type":"game"` etc.).
5. With the drop fault injection on (or `CLOSECIRCUIT` via tor's control port),
   a hand completes. A player closing their tab ends the game for the others
   within 120 s with a clear message.
6. The `p2skat-room` aarch64 binary is < 3 MB and < 20 MB RSS with one active
   room. The conformance suite passes against it on the router.

## Execution

- **Implementation, all phases (1–5):** Opus 5.5.
- **Review:** Fable 5.2, after Phase 2 (E2E crypto) and after Phase 3
  (reconnect and ordering). Each review is a fresh run with this plan, the
  diff and the open findings.
- **Phase 0 and router steps:** the user, or Opus 5.5 running unsandboxed
  (they need Tor Browser and SSH to the router).

## Review log

**v1 → v2, review by Fable 5.2.** Blockers fixed:

- acceptance 5 relied on "new circuit", which reloads the tab; it now uses
  fault injection or `CLOSECIRCUIT`;
- SSE reconnect couldn't work (a second stream got 409, there's no manual
  `Last-Event-ID`, heartbeat comments are invisible); now there's stream
  supersede, `?after=`, a named ping and a watchdog;
- strict "no replays" plus at-least-once resume would end games; duplicates
  are now dropped silently.

Important changes:

- one batched in-flight POST instead of 8 pipelined;
- server-side liveness timeout;
- a single acked log with router-sized limits;
- payload cap aligned with the event cap;
- an honest security rationale;
- mode bound into the fragment and MAC (downgrade);
- riskier Phase 0 checks added (Origin header, Wasm at Safer, router
  hello-world, real deal timing);
- prototype in `serve.py` before Rust;
- deployment details (opkg, ownership, procd user, sysupgrade);
- `relayPeers` branch instead of a `channelFactory`;
- a workspace that leaves the main build untouched;
- a conformance suite instead of porting the tests.
