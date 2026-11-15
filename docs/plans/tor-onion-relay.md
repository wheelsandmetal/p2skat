# Plan: Browser play over Tor through an onion relay on the router

Status: draft v2. The first review (Fable 5.2) has been incorporated; see
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
   across three Tor Browsers. A deal is about 15 sequential message hops
   (3 shuffle passes, 12 deal steps; see `src/session.rs:280-420`), so
   **expect 10–30 s**. If it's far worse, batching SRA rounds becomes a
   prerequisite, not a follow-up.
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
- Capability check on load: secure context, `crypto.subtle`, Wasm actually
  instantiates. Plus a `<noscript>`. Message: "In Tor Browser, set Security
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

## Risks and caveats

- **Weak uplink.** Mitigated by stream supersede, resume, the
  watchdog and the 120 s liveness window. The same binary and torrc run on any
  other always-on box if the router is too flaky.
- **Latency**: 10–30 s per deal expected (measured in Phase 0). Card play is
  fine.
- **The host serves the code.** E2E protects against a passive or curious host,
  not one serving modified JS/Wasm. Acceptable among friends; the README says
  so.
- **Metadata**: timing, counts and sizes are visible to the relay. No padding.
- **Reload ends the game**; no rejoin.
- **Onion key = table identity.** Losing it changes the address. Leaking it
  allows impersonation.

## Separate issue found during review (not part of this plan)

Card plaintexts are raw indices 1..32 (`src/crypto/protocol.rs`
`card_values()`). With SRA, a card's encryption preserves whether it is a
quadratic residue mod p, so every player learns that one bit of each
encrypted card. The standard fix is to map cards to quadratic residues (e.g.
encode card `i` as the i-th QR, or square each value). Track it separately. It
affects all transports.

## Out of scope

- Embedding arti in `p2skat-room` instead of system `tor`.
- Static hosting with public signaling (Nostr) for the WebRTC path.
- Rejoin after a reload.
- Batching SRA rounds, unless Phase 0 shows it's needed.
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
