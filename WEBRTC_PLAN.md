# Browser and WebRTC plan

Status: browser prototype implemented on `feat/browser-webrtc`; no hosting
provider chosen. Build and play instructions are in [README.md](README.md).

## Implementation checkpoint

- Shared Rust library and asynchronous session power both frontends.
- Browser Wasm build, secure browser randomness, and worker crypto self-check
  are implemented.
- Manual three-way WebRTC setup, optional TURN/relay-only configuration, and
  complete playable rounds are implemented.
- Bounded messages, sender/value validation, round agreement, disconnect
  handling, and regression tests are implemented.
- Native tests, simulation, and a three-terminal TCP/Ctrl+C smoke check passed.
  A real three-client browser test completed two rounds over direct connections
  with matching scores and responsive desktop/mobile viewport layouts. The same
  test passed through a local TURN server with relay-only policy; all six
  endpoints selected relay candidates. The desktop crypto self-check took
  roughly 6–11 seconds in headless Chromium on this test machine.

Still pending: real phone measurements, separate-network internet testing,
managed hosting/invite links, and an independent cryptographic review. The
inherited raw card encoding has known leakage (index 1 is unchanged by every
encryption layer), so this prototype cannot promise card secrecy or cheating
resistance. Hosting remains the later phase described below.

## Goal and scope

Make p2skat playable by three people in their browsers, in this repository.
Reuse the Rust game rules, scoring, and card encryption. Keep the existing
terminal app working with TCP and Tor.

The first version needs a simple card table, connection setup, and complete
rounds: shuffle, deal, bid, pick up/discard the skat, declare, play, reveal,
score, and agree to another round. A player leaving ends the game for everyone.

Skip accounts, matchmaking, spectators, chat, saved games, reconnection, and a
new repository. Start with plain HTML, CSS, and JavaScript in `web/`; add no UI
framework unless the implementation demonstrates a need.

## Repository structure

- `src/lib.rs`: shared Rust library, initially exposing `game` and `crypto`.
- `src/game/` and `src/crypto/`: one implementation used by both frontends.
- `src/net/`: existing native networking and the session orchestration to adapt.
- `src/ui/terminal.rs` and `src/main.rs`: native terminal frontend and CLI.
- `web/`: browser interface, Wasm loading, and native browser WebRTC APIs.
- `Cargo.toml` and `flake.nix`: browser build support alongside the native build.

Start with one Rust package. Isolate native dependencies and modules from the
Wasm build; a multi-crate workspace is unnecessary at this stage. Update CLI
and simulation imports to use the shared library rather than duplicate modules.

## 1. Prove the Rust core works in a browser

Expose the rules and crypto through `src/lib.rs`, add Wasm bindings, and build
for `wasm32-unknown-unknown`. Keep Tokio networking, Tor, SQLite, terminal input,
and CLI dependencies out of that build. Enable browser-backed secure randomness
for the existing `rand` dependency; never substitute `Math.random()`.

Add the Wasm target and required build tooling to the Nix development shell.
Load the module from a minimal page and exercise key generation, three-key
shuffle/deal, and final deck verification with the real 2048-bit prime.
Measure the time and responsiveness on a phone as well as a desktop. Run costly
crypto in a Web Worker if it blocks interaction, keeping keys on that client.

Done when the browser self-check passes, existing native tests and simulation
still pass, and the CLI still builds. This is the first feasibility gate,
before building the card table.

## 2. Separate session orchestration from terminal input and TCP

Read and adapt the existing flow in `src/net/session.rs`; keep the rules and
protocol logic shared instead of writing a second browser game loop.

Introduce only the boundaries both frontends need:

- Message send/receive for each remote seat. Keep the existing native framing;
  browser data channels carry complete serialized `Message` values.
- Asynchronous player choices and display updates. Replace direct `TerminalUi`
  calls and terminal quit polling in the shared flow with frontend input and
  cancellation. Adapt the terminal frontend to those same operations.

Keep the existing `Message` payloads where they fit. Native address discovery
messages such as `PeerListen` and `PeerConnect` remain native setup details.
Browser callbacks must retain incoming messages while the game awaits a player
choice; canceling a receive must not discard a message.

Pass only the local player's display information to the webpage. Private keys,
hidden cards, and internal crypto state stay on that player's device, apart
from protocol-required key revelation after the round.

Done when the shared session can run without a terminal and native behavior
still passes its existing checks.

## 3. Connect three browser peers

Create one reliable, ordered data channel between each pair of players: three
connections total, two per player. Assign seats once during setup and bind each
connection to its remote seat. Use a fixed seat order to choose which side
creates each connection's offer.

Start with manual copy/paste signaling to prove connectivity without building
a room service. Each pair exchanges an offer and answer after ICE candidate
collection completes. Explain each pairing in the setup UI. Start the session
only after all three pairwise channels are confirmed ready.

Use STUN for direct connection discovery. Test with TURN as well, including a
forced-relay connection. Without TURN, some networks will not connect; reliable
internet play requires that fallback. Check message sizes against the negotiated
data-channel limit, bound incoming payloads, and surface setup/send failures.

Done when three separate browser clients can shuffle, deal, and verify a deck
over real data channels, both directly and through TURN. Three tabs on one
machine are an initial smoke check, not the internet connectivity check.

## 4. Build the playable card table

Show the player's hand, current trick, whose turn it is, contract, round, and
scores in a stable layout. Add bidding, skat selection, declaration, legal card
selection, round results, and an explicit leave action. Support keyboard input,
visible focus, readable labels, and touch-sized controls.

Use Rust's legal actions to drive the controls. Keep local action validation in
the shared core. Synchronize each player's agreement to the next round before
starting a new shuffle. A disconnect or protocol error must end the session
with a clear explanation for the remaining players; resuming is out of scope.

Done when three browsers can play two complete rounds with matching scores,
fresh round keys, and successful reveal/verification, then leave cleanly.

## 5. Harden and verify the shared protocol before public use

Carry validation into the shared core rather than trusting the browser UI:

- Reject invalid cards, suits, seats, deck lengths, indices, and oversized
  numeric values with errors. Replace peer-controlled panics such as
  `CardMsg::to_card()` and update their callers.
- Validate sender identity against the connection, including the seat claimed
  by a bid. Reject messages that are invalid for the current phase or turn.
- Preserve fresh keys for every round and verification after play. Distinguish
  checking that a deck is a permutation from proving that every move was honest.

Add focused regression tests for malformed cards, forged bidding seats, and
messages received during pending input. Exercise the shared session through a
full round; `src/sim.rs` alone does not test the network/session flow. Retain the
native receive-cancellation regression test.

The existing custom encryption protocol needs a separate security review before
claiming cheating resistance or strong card secrecy. WebRTC transport encryption
does not establish those properties. Direct WebRTC connections also do not
preserve Tor mode's anonymity.

## 6. Choose hosting and simplify invitations

Choose hosting after the playable browser version works. The website is static
files, including Wasm. A signaling service only exchanges connection setup
information; gameplay and private keys stay on the clients. TURN may relay
encrypted traffic when a direct connection fails.

Manual setup remains sufficient for the prototype. For invite links, add a
small signaling service with unguessable room invitations, three-player room
limits, fixed seat ownership, message limits, and room expiry. Keep provider
secrets server-side; supply temporary TURN credentials to clients.

Cloudflare was discussed as a managed option. A home router was discussed
as a possible static/signaling host. Neither is selected: router hosting needs
its capabilities, HTTPS, external reachability, and separation from its admin
interface checked first. Do not make router-hosted TURN a requirement.

Done when an invite gets three players into a complete game across different
networks, including a relayed connection, and deployment instructions explain
the hosting and privacy limits.

## Delivery order

Wasm self-check -> shared session boundaries -> three-browser shuffle/deal ->
complete playable rounds -> public-use validation -> hosting and invite links.

Keep the native CLI usable at every stage. No deployment, provider account,
router changes, or additional product features are part of writing this plan.
