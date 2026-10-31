# p2skat

Three-player peer-to-peer Skat, with a native terminal app and a browser frontend.
Both frontends use the same Rust game session, rules, scoring, and card crypto.
The browser frontend uses WebRTC data channels; the native app uses TCP or Tor.

## Build and run the browser app

On NixOS, the development shell supplies Rust, the Wasm target, and the matching
`wasm-bindgen` CLI:

```sh
nix develop --command bash scripts/build-web.sh
nix develop --command python3 -m http.server 8080 --bind 127.0.0.1 --directory web
```

Open <http://localhost:8080>. The generated `web/pkg/` directory is ignored by
Git; rebuild it after changing Rust. The frontend has no JavaScript build step
or runtime package dependencies. For another machine, serve the contents of
`web/` over HTTPS with JavaScript and Wasm MIME types enabled. Localhost HTTP is
suitable for development.

### Connect three players

1. Agree on the three player names and assign seats 1, 2, and 3. Enter the same
   names in seat order on each device, select your own seat, and prepare the
   connections.
2. Pair seats 0 and 1, seats 0 and 2, then seats 1 and 2. For each pair, the lower
   seat generates an offer. Copy its connection details to the higher seat's
   input for that player and apply them.
3. The higher seat generates an answer. Copy its connection details back to the
   lower seat and apply them. Wait for each connection to open. The game starts
   after every player confirms both of their connections are ready.
4. Bid, select the skat and contract, and play cards using the table controls.
   Everyone must agree to continue before the next round starts. Leaving or
   losing a connection ends the session; reload to prepare a new game.

Copy details only when the page says they are ready; they include the complete
network candidate collection. Names and seat assignments must match on all
three devices. The prototype has manual signaling rather than room links.

### Networks and privacy

STUN helps discover direct routes. Some networks require a TURN relay; enter
your provider's TURN URL, username, and temporary credential in setup. The
relay-only option is useful for verifying that configuration. No relay service
or credentials are bundled. Never put a provider's API/admin secret in the
browser or repository. Keep the setup page and browser console out of shared
screenshots when credentials are present.

Game processing and private keys stay on each player's device, with keys
revealed by the protocol after a completed round. WebRTC encrypts transport;
direct peers can learn each other's network addresses. This browser mode does
not preserve Tor anonymity. The inherited custom card protocol is unaudited and
has known structure leakage: card index 1 remains 1 under every encryption
layer, revealing that card's position. This port does not guarantee card secrecy
or cheating resistance. Fixing the cryptographic design needs a separate review;
verifying the final deck alone is insufficient.

A self-hosted router and managed hosting are options for a later deployment, not
requirements to play. The implementation does not modify a router or create
external services. See [the implementation plan](WEBRTC_PLAN.md).

## Native app

```sh
nix develop
cargo run -- host
cargo run -- join 192.168.1.5:7878
cargo run -- host --tor
cargo run -- join example.onion:443 --tor
cargo run -- simulate --rounds 10
```

Use `--help` for the native connection options. TCP peers need reachable
addresses; Tor mode provides onion services.

## Checks

```sh
cargo test --locked
cargo run --locked -- simulate --rounds 10
nix develop --command bash scripts/build-web.sh
node --test web/*.test.mjs
```

For the three-browser smoke test, start the static server above and have
Chromium on your PATH (or set `CHROMIUM_PATH`). Install Playwright's driver in a
temporary directory; it is test tooling, not an app dependency:

```sh
npm install --prefix /tmp/p2skat-browser-test-tools --no-package-lock --no-audit --no-fund playwright-core
NODE_PATH=/tmp/p2skat-browser-test-tools/node_modules node scripts/browser-smoke.cjs http://localhost:8080
```

The smoke test runs the real crypto self-check, manually pairs three independent
clients, plays a pickup/Grand round and a Hand/Null round, compares scores, checks
round agreement and desktop/mobile layouts, and verifies leave propagation.
Set `TEST_TURN_URL`, `TEST_TURN_USERNAME`, and `TEST_TURN_CREDENTIAL` to repeat it
with relay-only connections; the test checks selected ICE candidates really use
the relay. It disables mDNS candidate masking only in the headless test browser
because local multicast may be unavailable in test environments.

The browser's crypto self-check exercises the real 2048-bit prime in a Web
Worker and reports its duration. Check it on the actual phones and desktops
you intend to use. A localhost three-browser test does not establish internet
or TURN connectivity; verify those with separate networks and valid relay
credentials before relying on them.
