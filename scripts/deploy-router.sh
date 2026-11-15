#!/usr/bin/env bash
# shellcheck disable=SC2029 # remote command strings are built locally on purpose
# Deploy p2skat-room behind a Tor onion service on an OpenWrt router
# (24.10: opkg, procd, busybox). Safe to re-run: it is also the restore path
# after a sysupgrade.
#
#   scripts/deploy-router.sh [--no-build] [--dry-run] SSH_HOST
#   scripts/deploy-router.sh --uninstall [--purge-keys] SSH_HOST
#   scripts/deploy-router.sh --check SSH_HOST
#
# SSH_HOST is anything ssh accepts (user@host, an alias from your ssh config).
# SSH_CONFIG=/path/to/config passes -F to ssh and scp. Router-side steps are in
# scripts/router-setup.sh, which runs as root on the router.
set -euo pipefail
cd "$(dirname "$0")/.."

usage() {
	sed -n '3,14s/^# \{0,1\}//p' "$0"
	exit "${1:-0}"
}

mode=install
build=1
dry_run=0
purge=
host=
while [[ $# -gt 0 ]]; do
	case $1 in
		--uninstall) mode=uninstall ;;
		--purge-keys) purge=--purge-keys ;;
		--check) mode=check ;;
		--no-build) build=0 ;;
		--dry-run) dry_run=1 ;;
		-h | --help) usage ;;
		-*) echo "Unknown option $1" >&2; usage 2 >&2 ;;
		*) [[ -z $host ]] || usage 2 >&2; host=$1 ;;
	esac
	shift
done
[[ -n $host ]] || usage 2 >&2
[[ -z $purge || $mode == uninstall ]] || { echo "--purge-keys goes with --uninstall" >&2; exit 2; }

binary=target/aarch64-unknown-linux-musl/router/p2skat-room
remote_dir=/tmp/p2skat-deploy
remote_setup=/tmp/p2skat-setup.sh

work=$(mktemp -d)
cleanup_steps=()
cleanup() {  # runs the registered steps last-in, first-out
	local i
	for ((i = ${#cleanup_steps[@]} - 1; i >= 0; i--)); do eval "${cleanup_steps[i]}" || true; done
	rm -rf "$work"
}
trap cleanup EXIT

# One SSH connection for the whole run, so a password is asked once.
ssh_opts=(-o ControlMaster=auto -o "ControlPath=$work/ssh-%C" -o ControlPersist=120)
[[ -z ${SSH_CONFIG:-} ]] || ssh_opts=(-F "$SSH_CONFIG" "${ssh_opts[@]}")

run() {  # print and run a local command; only print it with --dry-run
	printf '+ %s\n' "$*" >&2
	[[ $dry_run == 1 ]] || "$@"
}
remote() { run ssh "${ssh_opts[@]}" "$host" "$@"; }

close_master() { ssh "${ssh_opts[@]}" -O exit "$host" 2> /dev/null || true; }

in_dev_shell() {  # run a build script inside `nix develop` unless already in it
	if [[ -n ${IN_NIX_SHELL:-} ]]; then
		bash "$@"
	else
		nix develop --command bash "$@"
	fi
}

build_artifacts() {
	if [[ $build == 1 ]]; then
		in_dev_shell scripts/build-router.sh
		in_dev_shell scripts/build-web.sh
	fi
	local file
	for file in "$binary" web/index.html web/pkg/p2skat.js web/pkg/p2skat_bg.wasm; do
		[[ -f $file ]] || { echo "Missing $file; run without --no-build or build it first." >&2; exit 1; }
	done
}

# Only what the server serves: the app's html/css/js/mjs and web/pkg's js/wasm.
stage() {
	local dir=$work/p2skat-deploy file
	mkdir -p "$dir/web/pkg"
	cp "$binary" "$dir/p2skat-room"
	cp scripts/router-setup.sh "$dir/setup.sh"
	for file in web/*; do
		case $file in
			*.test.mjs) ;;
			*.html | *.css | *.js | *.mjs) cp "$file" "$dir/web/" ;;
		esac
	done
	cp web/pkg/*.js web/pkg/*.wasm "$dir/web/pkg/"
	tar -C "$work" --owner=0 --group=0 --numeric-owner -czf "$work/p2skat-deploy.tar.gz" p2skat-deploy
	echo "Staged $(du -k "$work/p2skat-deploy.tar.gz" | cut -f1) KiB:" >&2
	tar -tvzf "$work/p2skat-deploy.tar.gz" | awk '{ printf "  %8s %s\n", $3, $6 }' >&2
}

do_install() {
	build_artifacts
	stage
	cleanup_steps+=(close_master)
	remote "rm -rf $remote_dir $remote_dir.tar.gz"
	# OpenWrt's dropbear has no sftp server: scp -O uses the legacy protocol.
	run scp "${ssh_opts[@]}" -O "$work/p2skat-deploy.tar.gz" "$host:$remote_dir.tar.gz"
	remote "cd /tmp && tar -xzf $remote_dir.tar.gz && sh $remote_dir/setup.sh install $remote_dir < /dev/null; status=\$?; rm -rf $remote_dir $remote_dir.tar.gz; exit \$status"
	if [[ $dry_run == 1 ]]; then
		echo "Dry run: nothing was sent. Router script: scripts/router-setup.sh; generated files:" >&2
		sh scripts/router-setup.sh print
	fi
}

do_uninstall() {
	cleanup_steps+=(close_master)
	if [[ $dry_run == 1 ]]; then
		printf '+ ssh %s "cat > %s" < scripts/router-setup.sh\n' "$host" "$remote_setup" >&2
	else
		ssh "${ssh_opts[@]}" "$host" "cat > $remote_setup" < scripts/router-setup.sh
	fi
	remote "sh $remote_setup uninstall $purge < /dev/null; status=\$?; rm -f $remote_setup; exit \$status"
}

free_port() {
	python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'
}

wait_for_port() {
	local port=$1 tries=0
	until python3 -c 'import socket, sys; socket.create_connection(("127.0.0.1", int(sys.argv[1])), 1).close()' "$port" 2> /dev/null; do
		((++tries < 50)) || { echo "Tunnel to 127.0.0.1:$port did not come up" >&2; return 1; }
		sleep 0.2
	done
}

value() {  # value KEY TEXT: the value of a key=value line
	sed -n "s/^$1=//p" <<< "$2" | tail -n 1
}

# Acceptance criterion 6: the conformance suite against the deployed binary on
# the router, and its memory. The live server (--max-rooms 8) only gets the
# tests that create no tables, so real games are not disturbed; the full suite
# runs against a temporary second instance of the same binary, web files and
# --allowed-host, with the suite's short timings and the default table cap.
do_check() {
	[[ $dry_run == 0 ]] || { echo "--check has no dry run" >&2; exit 2; }
	command -v python3 > /dev/null || { echo "--check needs python3 (try: nix develop --command $0 --check $host)" >&2; exit 1; }
	cleanup_steps+=(close_master)
	ssh "${ssh_opts[@]}" "$host" "cat > $remote_setup" < scripts/router-setup.sh
	cleanup_steps+=("ssh \"\${ssh_opts[@]}\" \"\$host\" 'rm -f $remote_setup'")

	local status onion live_pid fast started test_pid test_port live_local test_local
	status=$(ssh "${ssh_opts[@]}" "$host" "sh $remote_setup status")
	onion=$(value onion "$status")
	live_pid=$(value pid "$status")
	[[ -n $onion && $(value configured_onion "$status") == "$onion" ]] ||
		{ echo "p2skat-room isn't deployed (or its allowed_host doesn't match the onion address):" >&2; echo "$status" >&2; exit 1; }
	[[ -n $live_pid ]] || { echo "p2skat-room isn't running on $host" >&2; echo "$status" >&2; exit 1; }

	fast=$(python3 -B scripts/test_serve.py --print-env)
	[[ $fast =~ ^(P2SKAT_[A-Z_]+=[0-9]+\ ?)+$ ]] || { echo "Unexpected --print-env output: $fast" >&2; exit 1; }
	started=$(ssh "${ssh_opts[@]}" "$host" "sh $remote_setup check-start '$fast'")
	test_pid=$(value pid "$started")
	test_port=$(value port "$started")
	cleanup_steps+=("ssh \"\${ssh_opts[@]}\" \"\$host\" 'sh $remote_setup check-stop $test_pid' > /dev/null 2>&1")

	live_local=$(free_port)
	test_local=$(free_port)
	ssh "${ssh_opts[@]}" -O forward -L "127.0.0.1:$live_local:127.0.0.1:8080" -L "127.0.0.1:$test_local:127.0.0.1:$test_port" "$host"
	wait_for_port "$live_local"
	wait_for_port "$test_local"

	local live_result=0 full_result=0 test_memory after
	echo "== Live server (--max-rooms 8), tests that create no tables" >&2
	P2SKAT_SERVER_URL=http://127.0.0.1:$live_local P2SKAT_SERVER_HOST=$onion \
		python3 -B scripts/test_serve.py StaticTests || live_result=$?
	echo "== Temporary instance on the router ($fast), full suite" >&2
	P2SKAT_SERVER_URL=http://127.0.0.1:$test_local P2SKAT_SERVER_HOST=$onion P2SKAT_SERVER_FAST=1 \
		python3 -B scripts/test_serve.py || full_result=$?

	test_memory=$(ssh "${ssh_opts[@]}" "$host" "sh $remote_setup check-stop $test_pid")
	after=$(ssh "${ssh_opts[@]}" "$host" "sh $remote_setup status")
	local bytes hwm verdict=PASS
	bytes=$(value binary_bytes "$after")
	hwm=$(value VmHWM_kB "$test_memory")
	[[ $live_result == 0 && $full_result == 0 ]] || verdict=FAIL
	[[ -n $bytes && $bytes -lt $((3 * 1024 * 1024)) ]] || verdict=FAIL
	[[ -n $hwm && $hwm -lt $((20 * 1024)) ]] || verdict=FAIL
	cat << EOF

Onion address:       http://$onion/
Binary:              $bytes bytes (limit 3 MiB)
Live server pid $(value pid "$after"): VmHWM $(value VmHWM_kB "$after") kB, VmRSS $(value VmRSS_kB "$after") kB
Test instance, after the full suite: VmHWM $hwm kB, VmRSS $(value VmRSS_kB "$test_memory") kB (limit 20 MiB)
Suite: live server $([[ $live_result == 0 ]] && echo passed || echo FAILED), full suite $([[ $full_result == 0 ]] && echo passed || echo FAILED)
Acceptance criterion 6: $verdict
EOF
	[[ $verdict == PASS ]]
}

case $mode in
	install) do_install ;;
	uninstall) do_uninstall ;;
	check) do_check ;;
esac
