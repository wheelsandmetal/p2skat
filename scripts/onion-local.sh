#!/usr/bin/env bash
# Serve p2skat over a throwaway Tor onion service from this computer, the same
# way the router does, for testing with Tor Browser without the router.
#
#   scripts/onion-local.sh [PORT]     (default port 8090; Ctrl-C stops both)
#
# Needs `tor` on PATH (e.g. nix shell nixpkgs#tor) and web/pkg from
# scripts/build-web.sh. Each run gets a new address; nothing is kept.
# P2SKAT_* environment knobs (fault injection, timings) reach the server.
set -euo pipefail
cd "$(dirname "$0")/.."
port=${1:-8090}
binary=${P2SKAT_ROOM_BIN:-target/release/p2skat-room}

command -v tor > /dev/null || { echo "Needs tor on PATH (try: nix shell nixpkgs#tor)" >&2; exit 1; }
[[ -f web/pkg/p2skat_bg.wasm ]] || { echo "Build the web app first: nix develop --command bash scripts/build-web.sh" >&2; exit 1; }
if [[ ! -x $binary ]]; then
	if [[ -n ${IN_NIX_SHELL:-} ]]; then cargo build --locked --release -p p2skat-room
	else nix develop --command cargo build --locked --release -p p2skat-room; fi
fi

dir=$(mktemp -d)
trap 'kill $(jobs -p) 2> /dev/null || true; wait 2> /dev/null || true; rm -rf "$dir"' EXIT
mkdir -m 700 "$dir/data" "$dir/hs"
cat > "$dir/torrc" << EOF
SocksPort 0
DataDirectory $dir/data
HiddenServiceDir $dir/hs
HiddenServicePort 80 127.0.0.1:$port
Log notice file $dir/tor.log
EOF
tor -f "$dir/torrc" > /dev/null &
for _ in $(seq 60); do
	[[ -s $dir/hs/hostname ]] && break
	kill -0 $! 2> /dev/null || { cat "$dir/tor.log" >&2; exit 1; }
	sleep 1
done
[[ -s $dir/hs/hostname ]] || { echo "tor did not create the onion address within 60 s; log:" >&2; cat "$dir/tor.log" >&2; exit 1; }
onion=$(< "$dir/hs/hostname")
echo "Open http://$onion/ in Tor Browser (publishing the service can take a minute)."
"$binary" --bind "127.0.0.1:$port" --web-dir web --allowed-host "$onion" --max-rooms 8
