# p2skat

Three-player peer-to-peer Skat, with a native terminal app and a browser frontend.
Both frontends use the same Rust game session, rules, scoring, and card crypto.
The browser frontend uses WebRTC between devices and local channels between
tabs in the same browser. The native app uses TCP or Tor.

## Build and run the browser app

On NixOS, the development shell supplies Rust, the Wasm target, and the matching
`wasm-bindgen` CLI:

```sh
nix develop --command bash scripts/build-web.sh
nix develop --command python3 scripts/serve.py
```

Open <http://localhost:8080>. The generated `web/pkg/` directory is ignored by
Git; rebuild it after changing Rust. The frontend has no JavaScript build step
or runtime package dependencies. The Python server serves the files and handles
invite setup using only the standard library. If the old `python3 -m http.server`
is still running, stop it with Ctrl+C before starting this server. Reload existing
tabs after switching servers.

### Connect three players

1. Enter your name and click **Create table**.
2. To try three tabs, click **Open another player tab** twice. Enter a name and
   click **Join table** in each new tab. All tabs must use the same site address
   and browser profile. No network settings or copied connection details are
   needed for this local test.
3. For other devices, share **Copy invite** with two friends. Each person opens
   the link, enters their own name, and joins. Seats are assigned automatically,
   and the game starts when all three players are connected.
4. Bid, select the skat and contract, and play cards using the table controls.
   Everyone must agree to continue before the next round starts. Leaving or
   losing a connection ends the session; reload to prepare a new game.

The server exchanges connection details, never cards or private keys. Established
games can keep running if the signaling connection drops. Rooms live in memory,
allow three players, and expire after an hour without activity. Restarting the
server loses its rooms; create a new table to invite new players.

For other devices, the server must be reachable over HTTPS, including its `/api/`
event streams. The default address is local to this computer. `--host` and
`--port` select a different listening address; this script does not configure
public hosting, HTTPS, or a TURN provider.

**Advanced · manual connection setup** remains available for static-only hosting.
Choose different seats and the same names on each device, then exchange the
offers and answers for pairs 1–2, 1–3, and 2–3.

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

A self-hosted router and managed hosting remain options for later deployment.
The implementation does not modify a router or create external services.
See [the implementation plan](WEBRTC_PLAN.md).

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
python3 -B -m unittest discover -s scripts -p 'test_serve.py' -v
```

For the three-browser smoke test, start the static server above and have
Chromium on your PATH (or set `CHROMIUM_PATH`). Install Playwright's driver in a
temporary directory; it is test tooling, not an app dependency:

```sh
npm install --prefix /tmp/p2skat-browser-test-tools --no-package-lock --no-audit --no-fund playwright-core
NODE_PATH=/tmp/p2skat-browser-test-tools/node_modules node scripts/browser-smoke.cjs http://localhost:8080
```

The default smoke test uses three tabs in one browser context, automatically
joins them through an invite, runs the real crypto self-check, plays a
pickup/Grand round and a Hand/Null round, compares scores, checks round agreement
and desktop/mobile layouts, and verifies leave propagation. It uses no WebRTC
override or pasted connection details for the same-browser case.
Set `TEST_WEBRTC=1` for automatic room setup across isolated browser profiles,
or `TEST_MANUAL=1` to exercise the advanced manual setup.
Set `TEST_TURN_URL`, `TEST_TURN_USERNAME`, and `TEST_TURN_CREDENTIAL` to repeat it
with relay-only connections; the test checks selected ICE candidates really use
the relay. Remote/manual headless tests disable mDNS candidate masking because
local multicast may be unavailable in test environments; the default same-browser
test does not need that workaround.

The browser's crypto self-check exercises the real 2048-bit prime in a Web
Worker and reports its duration. Check it on the actual phones and desktops
you intend to use. A localhost three-browser test does not establish internet
or TURN connectivity; verify those with separate networks and valid relay
credentials before relying on them.
