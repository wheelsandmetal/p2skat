# p2skat

Three-player peer-to-peer Skat, with a native terminal app and a browser frontend.
Both frontends use the same Rust game session, rules, scoring, and card crypto.
The browser frontend uses WebRTC between devices, local channels between
tabs in the same browser, or an end-to-end encrypted relay through the table
server for Tor Browser (see [Play over Tor](#play-over-tor-relay-mode)). The
native app uses TCP or Tor.

## Build and run the browser app

On NixOS, the development shell supplies Rust, the Wasm target, and the matching
`wasm-bindgen` CLI:

```sh
nix develop --command bash scripts/build-web.sh
nix develop --command cargo run --locked --release -p p2skat-room
```

Open <http://localhost:8080>. The generated `web/pkg/` directory is ignored by
Git; rebuild it after changing Rust. The frontend has no JavaScript build step
or runtime package dependencies. The room server (`room/`, a small Rust binary
independent of the game crate) serves the files and handles invite setup.
`python3 scripts/serve.py` is the standard-library prototype with the same API
and works the same way. Reload existing tabs after switching servers.

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

In WebRTC mode the server exchanges connection details, never cards or private
keys, and established games keep running if the signaling connection drops. Rooms live in memory,
allow three players, and expire after an hour without activity. Restarting the
server loses its rooms; create a new table to invite new players.

For other devices, the server must be reachable over HTTPS, including its `/api/`
event streams. The default address is local to this computer. `--bind` selects a
different listening address (`--host` and `--port` for `serve.py`), and
`--allowed-host` restricts the accepted `Host` header. The server does not
configure public hosting, HTTPS, or a TURN provider. See `p2skat-room --help`.

**Advanced → Manual connection** remains available for static-only hosting.
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
direct peers can learn each other's network addresses. WebRTC mode does not
preserve Tor anonymity; relay mode (below) does. The custom card protocol (SRA mental poker) is
unaudited. Cards are encoded as hash-derived quadratic residues, so ciphertexts
no longer show a card's quadratic-residue bit or the fixed point 1. No helper
decrypts ciphertexts for the player who chose them. After the key reveal every
player recomputes each partial decryption it received, so cheating is detected
after the hand. It is not prevented during play: two colluding players can make
the third strip a layer from values of their choice. This does not guarantee card
secrecy or cheating resistance; that needs zero-knowledge proofs of correct
decryption and a separate review.

All players need the same release: peers exchange a protocol version first and
refuse to play across versions.

To host a table as a Tor onion service on an OpenWrt router, see
[Host a table as an onion service](#host-a-table-as-an-onion-service). Nothing
else configures a router or creates external services.
See [the implementation plan](WEBRTC_PLAN.md).

## Play over Tor (relay mode)

Tor Browser has no WebRTC, so it can't connect players directly. Instead the
table server forwards the game messages, end-to-end encrypted, and nobody
learns another player's IP address when everyone uses Tor. The page picks relay
mode by itself when:

- the browser has no `RTCPeerConnection` (Tor Browser), or
- the page is served from an `.onion` address, or
- the invite link says so (`m=relay`).

In other browsers, **Network settings → Route game through the table server (no
direct connections)** creates a relay table, for testing or for players who
don't want to share IP addresses. STUN and TURN settings are hidden in relay
mode because nothing uses them.

A relay invite looks like `…/#room=<id>&k=<secret>&m=relay`. The part after `#`
never reaches the server. `k` is a 32-byte random secret that authenticates the
players' key exchange, so send the **whole** link privately; a link without it
is refused as incomplete. Anyone who has the link and also controls the server
could intercept the table, the same trust as WebRTC signaling.

In Tor Browser, set the **Security Level to Standard** for the site: the card
crypto runs in WebAssembly, which the Safer and Safest levels disable. The page
says so if WebAssembly is missing.

What the relay encryption gives you:

- each pair of players agrees on fresh keys (ephemeral ECDH P-256, bound to the
  invite secret, the table, both seats, all names and the relay mode by an
  HMAC), then exchanges AES-GCM frames. The server sees only ciphertext,
  sizes and timing;
- forward secrecy: keys live only in the open tab, so a link leaked after the
  game decrypts nothing;
- the server can't reorder, drop, replay or alter messages, or switch a relay
  table to WebRTC (which would reveal IP addresses), without the game noticing
  and ending.

It doesn't protect against a server that serves modified JavaScript or Wasm:
the host serves the code. Among friends running their own table server that is
the accepted trade-off.

Keep the tab open and in front. **Reloading the page, or a phone suspending the
tab for long, ends the game**: there is no rejoin. Short network hiccups are
fine: the page reconnects its event stream and resumes where it left off. If a
player has had no connection for 120 s, the server ends the table with
"<name> lost connection". Restarting the server ends all tables.

Over Tor each message takes about 0.4–1.4 s, so a deal takes several seconds
plus the card crypto; card play feels normal. The page shows progress while
keys are exchanged and cards are shuffled.

### Host a table as an onion service

The table server and a Tor onion service fit on a small always-on box: the
server is a static binary under 1 MB that serves the page and relays messages
on `127.0.0.1:8080`, and Tor publishes it as `http://<address>.onion/`. No
port forward is needed. `scripts/deploy-router.sh` sets this up over SSH on an
OpenWrt 24.10 router (aarch64, `opkg`, procd):

```sh
scripts/deploy-router.sh root@router            # build, copy, install; prints the onion address
scripts/deploy-router.sh --check root@router    # conformance suite and memory, through an SSH tunnel
scripts/deploy-router.sh --uninstall root@router
```

It builds the static `aarch64-unknown-linux-musl` server
(`scripts/build-router.sh`) and the web app inside `nix develop`
(`--no-build` uses existing builds; `--dry-run` builds and stages, prints the
commands and generated files, and sends nothing). It copies them with `scp -O`
(OpenWrt's dropbear has no SFTP). On the router it:

- installs `tor-basic` with `opkg` if tor is missing;
- creates an unprivileged `p2skat` account and installs `/usr/bin/p2skat-room`
  and `/usr/share/p2skat/web/`;
- adds a marked block to `/etc/tor/torrc` (`SocksPort 0`,
  `HiddenServiceDir /etc/tor/p2skat/`, `HiddenServicePort 80 127.0.0.1:8080`)
  and leaves the rest of the file alone;
- writes the onion address to `/etc/config/p2skat` and installs the procd
  service `/etc/init.d/p2skat-room` (runs as `p2skat`, respawns,
  `--allowed-host <address>.onion --max-rooms 8`);
- adds `/etc/tor/`, the init script and the config file to
  `/etc/sysupgrade.conf`, warns if NTP is off (Tor won't start with a wrong
  clock) and checks `/api/health`.

Running it again is safe. It restarts the server only if something changed,
and a restart ends every running game. If ssh rejects `~/.ssh/config` ("Bad
owner or permissions", seen in sandboxes), copy it to a private file and pass
`SSH_CONFIG=/path/to/copy`.

**Back up `/etc/tor/p2skat/` offline and keep it private.** It holds the onion
service's key, which is the table's identity: lose it and the address changes;
anyone who has it can impersonate the table.

```sh
ssh root@router 'tar -czf - -C / etc/tor/p2skat' > p2skat-onion-key.tgz
ssh root@router 'tar -xzf - -C /' < p2skat-onion-key.tgz    # restore, then deploy
```

A sysupgrade removes packages and `/usr` but keeps `/etc/tor/`. Run the deploy
script again afterwards to restore the table at the same address. To move the
table to another router, restore the key directory first.

`--uninstall` removes the service, its files, the torrc block and the
sysupgrade lines. It keeps the key unless you add `--purge-keys`, and leaves
`tor-basic` and the `p2skat` account installed.

`--check` needs `python3` (e.g. in `nix develop`). Through an SSH port forward
it runs `scripts/test_serve.py`'s tests that create no tables against the live
server, so running games aren't disturbed. Then it runs the full suite against
a temporary second instance of the deployed binary, with the suite's short
timings and the default table limit. It reports the binary size and the peak
memory (VmHWM) of both instances.

`scripts/onion-local.sh` does the same on a laptop with the system `tor` and a
throwaway address, to try Tor Browser without a router.

Hosting a table means:

- **you serve the code.** The relay encryption protects against a curious
  host, not one that changes the JavaScript or Wasm it serves;
- the server sees the players' names, message sizes and timing (there's no
  padding), though not the cards or anyone's IP address;
- reloading a tab ends that game, and restarting the server (or the router)
  ends all games. Tables live only in memory.

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
cargo test -p p2skat-room
python3 -B scripts/test_serve.py                    # conformance suite against serve.py
cargo build -p p2skat-room && P2SKAT_SERVER=rust python3 -B scripts/test_serve.py
```

`test_serve.py` is a black-box HTTP/SSE suite. It starts a fresh server per
test with short timings; `P2SKAT_SERVER_URL=http://host:port` targets a running
server instead and skips tests that need a special configuration.

`scripts/build-router.sh` (inside `nix develop`) builds a static
`aarch64-unknown-linux-musl` room server for the OpenWrt router;
`scripts/deploy-router.sh --check` runs the suite against it there (see
[Host a table as an onion service](#host-a-table-as-an-onion-service)).
`P2SKAT_SERVER_HOST=<name>` makes the suite send that `Host` header, for a
server started with `--allowed-host`.

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
`--relay` (or `TEST_RELAY=1`) plays the same two rounds in relay mode across
isolated profiles with `RTCPeerConnection` deleted, as in Tor Browser. It
checks that every relay payload sent or received is a key frame or ciphertext,
never a plaintext packet, then closes one tab and expects the others to end the
game. Start the server with a short `P2SKAT_LIVENESS_MS` and pass the same value
as `TEST_LIVENESS_MS` to also block that tab's leave request and test the
server's liveness timeout. For the fault-injection runs, start the server with
`P2SKAT_FAULT_LATENCY_MS=800,400` or `P2SKAT_FAULT_DROP_S=15` and raise the
deadline with `TEST_DEADLINE_S=900`:

```sh
P2SKAT_FAULT_LATENCY_MS=800,400 P2SKAT_LIVENESS_MS=10000 target/debug/p2skat-room --bind 127.0.0.1:8793 &
TEST_DEADLINE_S=900 TEST_LIVENESS_MS=10000 NODE_PATH=/tmp/p2skat-browser-test-tools/node_modules \
  node scripts/browser-smoke.cjs --relay http://127.0.0.1:8793
```
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
