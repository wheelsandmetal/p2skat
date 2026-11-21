#!/usr/bin/env bash
# Pick and run the checks a change needs, from the files it touches.
#   scripts/verify.sh                  gates for files changed since the base (default origin/master)
#   scripts/verify.sh --base REF       compare against REF instead
#   scripts/verify.sh --all            every gate
#   scripts/verify.sh GATE...          exactly these gates (rust wasm web serve room browser faults)
#   scripts/verify.sh --list           print the selected gates and the files that chose them, run nothing
# Logs go to target/verify/<timestamp>/<gate>.log. Exit status is non-zero if any gate failed.
set -uo pipefail
cd "$(dirname "$0")/.." || exit 2

if ! command -v cargo > /dev/null || ! command -v python3 > /dev/null; then
	exec nix develop --command "$PWD/scripts/verify.sh" "$@"
fi

GATES=(rust wasm web serve room browser faults)
declare -A TRIGGER=(
	[rust]='^(src/|Cargo\.(toml|lock)$)'
	[wasm]='^(src/|Cargo\.(toml|lock)$|scripts/build-web\.sh$)'
	[web]='^web/'
	[serve]='^scripts/(serve|test_serve)\.py$'
	[room]='^(room/|scripts/test_serve\.py$|Cargo\.lock$)'
	[browser]='^(src/(session\.rs|wasm\.rs|crypto/|game/|net/message\.rs)|web/|room/|scripts/(browser-smoke\.cjs|build-web\.sh)$|Cargo\.(toml|lock)$)'
	[faults]='^(web/(relay|transport)\.mjs|room/src/(rooms|http)\.rs|scripts/browser-smoke\.cjs)$'
)
declare -A ABOUT=(
	[rust]='cargo test, 10 simulated rounds'
	[wasm]='Wasm build into web/pkg'
	[web]='node --test web/*.test.mjs'
	[serve]='conformance suite against serve.py'
	[room]='room unit tests, conformance suite against p2skat-room'
	[browser]='fresh web/pkg, three-tab smoke test, relay smoke test'
	[faults]='relay smoke test under latency, then under a 15 s drop'
)
PLAYWRIGHT_DIR=/tmp/p2skat-browser-test-tools

gate_rust() {
	cargo test --locked && cargo run --locked -- simulate --rounds 10
}
gate_wasm() {
	bash scripts/build-web.sh
}
gate_web() {
	node --test web/*.test.mjs
}
gate_serve() {
	python3 -B scripts/test_serve.py
}
gate_room() {
	cargo test --locked -p p2skat-room &&
		cargo build --locked -p p2skat-room &&
		P2SKAT_SERVER=rust python3 -B scripts/test_serve.py
}

free_port() {
	python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'
}
# Runs "$@" against a fresh p2skat-room started with the P2SKAT_* variables already exported.
with_room() {
	local port pid result
	port=$(free_port)
	target/debug/p2skat-room --bind "127.0.0.1:$port" &
	pid=$!
	for _ in $(seq 50); do
		(: < "/dev/tcp/127.0.0.1/$port") 2> /dev/null && break
		sleep 0.1
	done
	"$@" "http://127.0.0.1:$port"
	result=$?
	kill "$pid"
	wait "$pid" 2> /dev/null
	return "$result"
}
browser_prereqs() {
	[[ -d $PLAYWRIGHT_DIR/node_modules/playwright-core ]] ||
		npm install --prefix "$PLAYWRIGHT_DIR" --no-package-lock --no-audit --no-fund playwright-core || return
	command -v chromium > /dev/null || [[ -n ${CHROMIUM_PATH:-} ]] || { echo 'No chromium on PATH; set CHROMIUM_PATH.'; return 1; }
	gate_wasm && cargo build --locked -p p2skat-room
}
smoke() {
	NODE_PATH=$PLAYWRIGHT_DIR/node_modules node scripts/browser-smoke.cjs "$@"
}
gate_browser() {
	browser_prereqs || return
	with_room smoke && with_room smoke --relay
}
gate_faults() {
	browser_prereqs || return
	export P2SKAT_LIVENESS_MS=10000 TEST_LIVENESS_MS=10000 TEST_DEADLINE_S=900
	P2SKAT_FAULT_LATENCY_MS=800,400 with_room smoke --relay &&
		P2SKAT_FAULT_DROP_S=15 with_room smoke --relay
}

base=
mode="diff"
list_only=0
selected=()
while (($#)); do
	case $1 in
		--base) base=$2; shift ;;
		--all) mode=all ;;
		--list) list_only=1 ;;
		-h | --help) sed -n '2,8s/^# \{0,1\}//p' "$0"; exit 0 ;;
		*)
			[[ -v "TRIGGER[$1]" ]] || { echo "Unknown gate '$1'. Gates: ${GATES[*]}" >&2; exit 2; }
			mode=named
			selected+=("$1")
			;;
	esac
	shift
done

declare -A reason=()
if [[ $mode == diff ]]; then
	if [[ -z $base ]]; then
		base=origin/master
		git rev-parse -q --verify "$base" > /dev/null || base=master
	fi
	merge_base=$(git merge-base HEAD "$base") || { echo "No merge base with $base" >&2; exit 2; }
	mapfile -t changed < <({ git diff --name-only "$merge_base"; git ls-files --others --exclude-standard; } | sort -u)
	echo "Changed since $base (${merge_base:0:7}): ${#changed[@]} file(s)"
	for gate in "${GATES[@]}"; do
		for file in "${changed[@]}"; do
			if [[ $file =~ ${TRIGGER[$gate]} ]]; then
				selected+=("$gate")
				reason[$gate]=$file
				break
			fi
		done
	done
elif [[ $mode == all ]]; then
	selected=("${GATES[@]}")
fi

if ((${#selected[@]} == 0)); then
	echo 'No gate covers these files. Name gates or pass --all to run checks anyway.'
	exit 0
fi
for gate in "${selected[@]}"; do
	printf '  %-8s %s%s\n' "$gate" "${ABOUT[$gate]}" "${reason[$gate]:+  (because of ${reason[$gate]})}"
done
((list_only)) && exit 0

run_dir=target/verify/$(date +%Y%m%d-%H%M%S)
mkdir -p "$run_dir"
echo "Logs: $run_dir"
failed=0
for gate in "${selected[@]}"; do
	start=$SECONDS
	("gate_$gate") > "$run_dir/$gate.log" 2>&1
	result=$?
	status=PASS
	((result == 0)) || { status=FAIL; failed=1; }
	printf '%s %-8s %4ss  %s\n' "$status" "$gate" $((SECONDS - start)) "$run_dir/$gate.log" | tee -a "$run_dir/summary.txt"
	((result == 0)) || tail -n 15 "$run_dir/$gate.log" | sed 's/^/    /'
done
git rev-parse HEAD >> "$run_dir/summary.txt"
git status --short >> "$run_dir/summary.txt"
exit "$failed"
