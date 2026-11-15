#!/bin/sh
# Router side of scripts/deploy-router.sh: p2skat-room behind a Tor onion
# service on OpenWrt (busybox ash, opkg, procd). deploy-router.sh copies this
# file to the router and runs it as root; every command is idempotent.
#
#   sh router-setup.sh install STAGING_DIR    STAGING_DIR holds p2skat-room and web/
#   sh router-setup.sh uninstall [--purge-keys]
#   sh router-setup.sh status                 key=value lines (onion, pid, memory)
#   sh router-setup.sh check-start 'P2SKAT_X=1 ...'   temporary test instance
#   sh router-setup.sh check-stop PID
#   sh router-setup.sh print                  show the generated files
#
# P2SKAT_ROOT (tests only) prefixes every file path, so the script can run
# against a scratch directory with stub commands on PATH.
set -eu

R=${P2SKAT_ROOT:-}
PORT=8080
MAX_ROOMS=8
SERVICE=p2skat-room
ACCOUNT=p2skat
HS_DIR=/etc/tor/p2skat
BIN=/usr/bin/p2skat-room
WEB=/usr/share/p2skat/web
INIT=/etc/init.d/p2skat-room
CONF=/etc/config/p2skat
SYSUPGRADE=/etc/sysupgrade.conf
BEGIN="# BEGIN p2skat (managed by scripts/deploy-router.sh)"
END="# END p2skat"
CHECK_LOG=$R/tmp/p2skat-check.log

log() { printf '==> %s\n' "$*"; }
warn() { printf 'WARNING: %s\n' "$*" >&2; }
die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

checksum() { md5sum | cut -d' ' -f1; }

# rc NAME ACTION: what running /etc/init.d/NAME ACTION does through its
# `#!/bin/sh /etc/rc.common` line, spelled out so tests can stub rc.common.
rc() { /bin/sh "$R/etc/rc.common" "$R/etc/init.d/$1" "$2"; }

# Checksum of every file under a directory (names and contents).
tree_sum() {
	[ -d "$1" ] || { echo none; return; }
	(cd "$1" && find . -type f | sort | while read -r file; do
		printf '%s %s\n' "$file" "$(checksum < "$file")"
	done) | checksum
}

file_sum() { if [ -f "$1" ]; then checksum < "$1"; else echo none; fi; }

# The torrc that tor reads: OpenWrt's init script includes tor.conf.default
# (normally /etc/tor/torrc) in the configuration it generates.
torrc_path() {
	path=$(uci -q get tor.conf.default 2>/dev/null || true)
	echo "${path:-/etc/tor/torrc}"
}

# Last value of a torrc option. Option names are case-insensitive.
torrc_value() {
	[ -f "$2" ] || return 0
	awk -v key="$1" 'tolower($1) == tolower(key) { value = $2 } END { if (value != "") print value }' "$2"
}

# Print FILE without the marked p2skat block.
strip_block() {
	awk -v begin="$BEGIN" -v end="$END" '
		$0 == begin { skip = 1; next }
		skip && $0 == end { skip = 0; next }
		!skip { print }' "$1"
}

# set_block FILE CONTENT: replace the marked block in FILE with CONTENT, append
# it if missing, remove it if CONTENT is empty. Lines outside the block are kept.
# Sets block_changed to 1 if FILE changed, else 0. (Functions report through
# variables, not exit codes: `if f` would switch off set -e inside f.)
set_block() {
	file=$1
	content=$2
	block_changed=0
	[ -f "$file" ] || { [ -n "$content" ] || return 0; : > "$file"; }
	begins=$(grep -cxF "$BEGIN" "$file" || true)
	ends=$(grep -cxF "$END" "$file" || true)
	if [ "$begins" != "$ends" ] || [ "$begins" -gt 1 ]; then
		die "$file has unbalanced or repeated p2skat marker lines; fix it by hand"
	fi
	new="$file.p2skat-new"
	strip_block "$file" > "$new"
	[ -z "$content" ] || printf '%s\n%s\n%s\n' "$BEGIN" "$content" "$END" >> "$new"
	if [ "$(file_sum "$file")" = "$(file_sum "$new")" ]; then
		rm -f "$new"
		return 0
	fi
	cat "$new" > "$file"  # keeps the file's owner and mode
	rm -f "$new"
	block_changed=1
}

tor_running() { pidof tor > /dev/null 2>&1; }

# The account tor runs as: `User` in torrc, else procd's user in the init
# script, else root. The onion service directory must belong to it.
tor_account() {
	account=$(torrc_value User "$R$(torrc_path)")
	if [ -z "$account" ] && [ -f "$R/etc/init.d/tor" ]; then
		account=$(sed -n 's/.*procd_set_param[[:space:]]\{1,\}user[[:space:]]\{1,\}\([A-Za-z0-9_-]*\).*/\1/p' \
			"$R/etc/init.d/tor" | tail -n 1)
	fi
	echo "${account:-root}"
}

onion_address() {
	[ -s "$R$HS_DIR/hostname" ] || return 0
	tr -d ' \r\n' < "$R$HS_DIR/hostname"
}

valid_onion() {
	host=${1%.onion}
	[ "$host.onion" = "$1" ] && [ "${#host}" -eq 56 ] || return 1
	case $host in *[!a-z2-7]*) return 1 ;; esac
}

configured_onion() {
	[ -f "$R$CONF" ] || return 0
	sed -n "s/^[[:space:]]*option[[:space:]]\{1,\}allowed_host[[:space:]]\{1,\}'\{0,1\}\([^' ]*\)'\{0,1\}.*/\1/p" "$R$CONF" | tail -n 1
}

server_pid() {
	for pid in $(pidof "$SERVICE" 2> /dev/null || true); do
		# The procd instance runs as p2skat; a check-start instance runs as root.
		if grep -q "^Uid:[[:space:]]*$(id -u "$ACCOUNT" 2> /dev/null || echo none)[[:space:]]" "/proc/$pid/status" 2> /dev/null; then
			echo "$pid"
			return
		fi
	done
}

memory() {  # memory PID: VmHWM and VmRSS from /proc/PID/status as key=value lines
	awk '$1 == "VmHWM:" || $1 == "VmRSS:" { sub(":", "", $1); print $1 "_kB=" $2 }' "/proc/$1/status" 2> /dev/null || true
}

init_script() {
	cat << 'EOF'
#!/bin/sh /etc/rc.common
# p2skat-room: Skat tables behind the Tor onion service in /etc/tor/p2skat/.
# Installed by scripts/deploy-router.sh; settings in /etc/config/p2skat.
START=95
STOP=10
USE_PROCD=1
PROG=/usr/bin/p2skat-room

start_service() {
	local bind web_dir allowed_host max_rooms
	config_load p2skat
	config_get bind server bind 127.0.0.1:8080
	config_get web_dir server web_dir /usr/share/p2skat/web
	config_get allowed_host server allowed_host
	config_get max_rooms server max_rooms 8
	if [ ! -x "$PROG" ]; then
		echo "p2skat-room: $PROG is missing (sysupgrade?); re-run scripts/deploy-router.sh" >&2
		return 1
	fi
	if [ -z "$allowed_host" ]; then
		echo "p2skat-room: no allowed_host in /etc/config/p2skat" >&2
		return 1
	fi
	procd_open_instance
	procd_set_param command "$PROG" --bind "$bind" --web-dir "$web_dir" \
		--allowed-host "$allowed_host" --max-rooms "$max_rooms" \
		--max-connections 96 --relay-only
	procd_set_param user p2skat
	procd_set_param no_new_privs 1
	procd_set_param respawn 3600 5 0
	procd_set_param stdout 1
	procd_set_param stderr 1
	procd_close_instance
}

service_triggers() {
	procd_add_reload_trigger p2skat
}
EOF
}

server_config() {
	cat << EOF
# Written by scripts/deploy-router.sh; re-running it rewrites this file.
config server 'server'
	option bind '127.0.0.1:$PORT'
	option web_dir '$WEB'
	option allowed_host '$1'
	option max_rooms '$MAX_ROOMS'
EOF
}

sysupgrade_lines() {
	printf '%s\n' /etc/tor/ "$INIT" "$CONF"
}

ensure_tor() {
	if [ -x "$R/usr/sbin/tor" ] && [ -f "$R/etc/init.d/tor" ]; then
		return
	fi
	log "Installing tor-basic"
	opkg update
	opkg install tor-basic
	[ -x "$R/usr/sbin/tor" ] && [ -f "$R/etc/init.d/tor" ] || die "tor-basic did not install /usr/sbin/tor and /etc/init.d/tor"
}

check_clock() {
	if [ "$(uci -q get system.ntp.enabled 2> /dev/null || true)" != 1 ]; then
		warn "NTP looks disabled (system.ntp.enabled != 1). Tor won't bootstrap with a wrong clock:"
		warn "  uci set system.ntp.enabled=1 && uci commit system && /etc/init.d/sysntpd restart"
	fi
	log "Router clock: $(date -u '+%Y-%m-%d %H:%M:%S UTC')"
}

next_free_id() {  # next_free_id FILE START: lowest numeric id >= START unused in FILE's third field
	id=$2
	while cut -d: -f3 "$1" | grep -qx "$id"; do id=$((id + 1)); done
	echo "$id"
}

# procd's `user` needs the account in /etc/passwd (kept by sysupgrade).
ensure_account() {
	if grep -q "^$ACCOUNT:" "$R/etc/passwd" && grep -q "^$ACCOUNT:" "$R/etc/group"; then
		return
	fi
	log "Creating user and group $ACCOUNT"
	if [ -f "$R/lib/functions.sh" ]; then
		# OpenWrt's helpers (with /var/lock locking); they don't run under set -u.
		(
			set +eu
			# shellcheck disable=SC2034 # read by functions.sh
			IPKG_INSTROOT=$R
			# shellcheck disable=SC1091
			. "$R/lib/functions.sh"
			gid=$(group_add_next "$ACCOUNT")
			[ -n "$gid" ] || exit 1
			if ! user_exists "$ACCOUNT"; then
				uid=$(next_free_id "$R/etc/passwd" "$gid")
				user_add "$ACCOUNT" "$uid" "$gid" "$ACCOUNT" /var/run/$ACCOUNT /bin/false || exit 1
			fi
		) || die "could not create the $ACCOUNT account"
	else
		gid=$(sed -n "s/^$ACCOUNT:[^:]*:\([0-9]*\):.*/\1/p" "$R/etc/group")
		if [ -z "$gid" ]; then
			gid=$(next_free_id "$R/etc/group" 32768)
			echo "$ACCOUNT:x:$gid:" >> "$R/etc/group"
		fi
		if ! grep -q "^$ACCOUNT:" "$R/etc/passwd"; then
			uid=$(next_free_id "$R/etc/passwd" "$gid")
			echo "$ACCOUNT:x:$uid:$gid:$ACCOUNT:/var/run/$ACCOUNT:/bin/false" >> "$R/etc/passwd"
			[ ! -f "$R/etc/shadow" ] || echo "$ACCOUNT:*:0:0:99999:7:::" >> "$R/etc/shadow"
		fi
	fi
	grep -q "^$ACCOUNT:" "$R/etc/passwd" || die "could not create the $ACCOUNT account"
}

install_files() {  # sets files_changed
	stage=$1
	files_changed=0
	if [ "$(file_sum "$stage/p2skat-room")" = "$(file_sum "$R$BIN")" ] &&
		[ "$(tree_sum "$stage/web")" = "$(tree_sum "$R$WEB")" ]; then
		log "Binary and web files are up to date"
		return 0
	fi
	files_changed=1
	log "Installing $BIN and $WEB/"
	mkdir -p "$R${BIN%/*}" "$R${WEB%/*}"
	cp "$stage/p2skat-room" "$R$BIN.new"
	chmod 755 "$R$BIN.new"
	mv -f "$R$BIN.new" "$R$BIN"
	rm -rf "$R$WEB.new" "$R$WEB.old"
	cp -r "$stage/web" "$R$WEB.new"
	find "$R$WEB.new" -type d -exec chmod 755 {} \;
	find "$R$WEB.new" -type f -exec chmod 644 {} \;
	[ ! -d "$R$WEB" ] || mv "$R$WEB" "$R$WEB.old"
	mv "$R$WEB.new" "$R$WEB"
	rm -rf "$R$WEB.old"
}

torrc_block() {
	torrc=$1
	if strip_block "$torrc" 2> /dev/null | grep -qi "^[[:space:]]*HiddenServiceDir[[:space:]]\{1,\}$HS_DIR/\{0,1\}[[:space:]]*$"; then
		die "$torrc already configures $HS_DIR outside the p2skat block; remove that by hand"
	fi
	if strip_block "$torrc" 2> /dev/null | grep -qi '^[[:space:]]*SocksPort[[:space:]]'; then
		warn "$torrc has its own SocksPort line; leaving it (the plan uses SocksPort 0)"
	else
		echo "SocksPort 0"
	fi
	echo "HiddenServiceDir $HS_DIR/"
	echo "HiddenServicePort 80 127.0.0.1:$PORT"
}

configure_tor() {
	torrc=$(torrc_path)
	mkdir -p "$R${torrc%/*}"
	block=$(torrc_block "$R$torrc")
	set_block "$R$torrc" "$block"
	tor_changed=$block_changed
	[ "$tor_changed" = 0 ] || log "Updated the p2skat block in $torrc"
	account=$(tor_account)
	[ "$account" != root ] || warn "tor runs as root (no User in $torrc); $HS_DIR will belong to root"
	grep -q "^$account:" "$R/etc/passwd" || die "tor's account '$account' is not in /etc/passwd"
	mkdir -p "$R$HS_DIR"
	chown -R "$account" "$R$HS_DIR"
	chmod -R go-rwx "$R$HS_DIR"
	chmod 700 "$R$HS_DIR"
	# tor checks the directory's owner against its User option, so a root
	# --verify-config only means something if torrc names the account.
	if [ "$tor_changed" = 1 ] && { [ "$account" = root ] || [ "$(torrc_value User "$R$torrc")" = "$account" ]; }; then
		if ! output=$("$R/usr/sbin/tor" --verify-config -f "$R$torrc" 2>&1); then
			printf '%s\n' "$output" >&2
			die "tor rejects $torrc (see above); the p2skat block is in place, fix the rest by hand"
		fi
	fi
	rc tor enable
	if [ "$tor_changed" = 1 ] || ! tor_running; then
		log "Restarting tor"
		rc tor restart
	fi
	waited=0
	until [ -s "$R$HS_DIR/hostname" ]; do
		[ "$waited" -lt 60 ] || die "tor did not create $HS_DIR/hostname within 60 s; see: logread -e tor"
		sleep 1
		waited=$((waited + 1))
	done
	sleep 2
	tor_running || die "tor is not running; see: logread -e tor"
}

health() {
	onion=$1
	url="http://127.0.0.1:$PORT/api/health"
	tries=0
	while [ "$tries" -lt 10 ]; do
		tries=$((tries + 1))
		# The server only answers its onion name (--allowed-host). OpenWrt's
		# default wget (uclient-fetch) can't set Host, so fall back to nc, and
		# finally accept the server's 421 for a foreign Host as a sign of life.
		body=$(wget -q -T 5 -O - --header "Host: $onion" "$url" 2> /dev/null || true)
		if [ -z "$body" ] && command -v nc > /dev/null 2>&1; then
			body=$(printf 'GET /api/health HTTP/1.0\r\nHost: %s\r\n\r\n' "$onion" | nc 127.0.0.1 "$PORT" 2> /dev/null || true)
		fi
		case $body in *'"ok":true'*)
			log "Health check passed: /api/health answered for $onion"
			return 0
			;;
		esac
		if wget -T 5 -O /dev/null "$url" 2>&1 | grep -q ' 421'; then
			log "Health check: server answers (421 for a non-onion Host, as configured)"
			return 0
		fi
		sleep 1
	done
	return 1
}

do_install() {
	stage=${1:?install needs the staging directory}
	[ -n "$R" ] || [ "$(id -u)" = 0 ] || die "run as root"
	[ -f "$stage/p2skat-room" ] && [ -f "$stage/web/index.html" ] && [ -d "$stage/web/pkg" ] ||
		die "$stage must contain p2skat-room, web/index.html and web/pkg/"
	chmod 755 "$stage/p2skat-room"
	"$stage/p2skat-room" --help > /dev/null 2>&1 ||
		die "the staged p2skat-room doesn't run here (wrong architecture? build with scripts/build-router.sh)"

	check_clock
	ensure_tor
	ensure_account
	install_files "$stage"
	server_changed=$files_changed

	configure_tor
	onion=$(onion_address)
	valid_onion "$onion" || die "$HS_DIR/hostname doesn't hold a v3 onion address: '$onion'"

	mkdir -p "$R${CONF%/*}" "$R${INIT%/*}"
	if [ "$(server_config "$onion" | checksum)" != "$(file_sum "$R$CONF")" ]; then
		server_config "$onion" > "$R$CONF"
		server_changed=1
	fi
	if [ "$(init_script | checksum)" != "$(file_sum "$R$INIT")" ]; then
		init_script > "$R$INIT"
		server_changed=1
	fi
	chmod 755 "$R$INIT"
	set_block "$R$SYSUPGRADE" "$(sysupgrade_lines)"
	[ "$block_changed" = 0 ] || log "Added /etc/tor/, $INIT and $CONF to $SYSUPGRADE"

	rc "$SERVICE" enable
	if [ "$server_changed" = 1 ] || [ -z "$(server_pid)" ]; then
		log "Restarting $SERVICE (this ends any running games)"
		rc "$SERVICE" restart
	else
		log "$SERVICE is running and unchanged; not restarting it"
	fi
	health "$onion" || die "p2skat-room does not answer on 127.0.0.1:$PORT; see: logread -e p2skat"

	pid=$(server_pid)
	[ -z "$pid" ] || log "p2skat-room pid $pid: $(memory "$pid" | tr '\n' ' ')"
	log "Done. Onion address: http://$onion/"
	log "Back up $HS_DIR/ offline: it is the table's identity (address and key)."
}

do_uninstall() {
	purge=0
	[ "${1:-}" != --purge-keys ] || purge=1
	[ -n "$R" ] || [ "$(id -u)" = 0 ] || die "run as root"
	if [ -f "$R$INIT" ]; then
		log "Stopping and disabling $SERVICE"
		rc "$SERVICE" stop || true
		rc "$SERVICE" disable || true
	fi
	rm -f "$R$INIT" "$R$CONF" "$R$BIN" "$R$BIN.new"
	rm -rf "$R${WEB%/*}"
	torrc=$(torrc_path)
	set_block "$R$torrc" ""
	if [ "$block_changed" = 1 ]; then
		log "Removed the p2skat block from $torrc"
		if tor_running; then
			log "Restarting tor so it stops publishing the onion service"
			rc tor restart
		fi
	fi
	set_block "$R$SYSUPGRADE" ""
	[ "$block_changed" = 0 ] || log "Removed the p2skat lines from $SYSUPGRADE"
	if [ "$purge" = 1 ]; then
		rm -rf "$R$HS_DIR"
		warn "Deleted $HS_DIR: the onion address is gone for good unless you have a backup."
	elif [ -d "$R$HS_DIR" ]; then
		warn "Kept the onion keys in $HS_DIR (address $(onion_address))."
		warn "sysupgrade no longer preserves them; back them up, or pass --purge-keys to delete them."
	fi
	log "Left installed: tor-basic (opkg remove tor-basic) and the $ACCOUNT account in /etc/passwd."
}

do_status() {
	echo "onion=$(onion_address)"
	echo "configured_onion=$(configured_onion)"
	[ ! -f "$R$BIN" ] || echo "binary_bytes=$(wc -c < "$R$BIN" | tr -d ' ')"
	pid=$(server_pid)
	echo "pid=$pid"
	[ -z "$pid" ] || memory "$pid"
	if tor_running; then echo "tor=running"; else echo "tor=stopped"; fi
}

do_check_start() {
	settings=${1:-}
	for setting in $settings; do
		case $setting in P2SKAT_[A-Z_]*=[0-9]*) ;; *) die "bad setting '$setting'" ;; esac
	done
	onion=$(configured_onion)
	[ -n "$onion" ] || die "p2skat-room isn't installed (no allowed_host in $CONF)"
	rm -f "$CHECK_LOG"
	set --
	if command -v setsid > /dev/null 2>&1; then set -- setsid; fi
	# The production settings except --max-rooms (the suite needs more than 8
	# tables) and the short timings in $settings. Runs as root, loopback only,
	# until check-stop.
	# shellcheck disable=SC2086 # $settings is a validated list of NAME=VALUE words
	env $settings "$@" "$R$BIN" --bind 127.0.0.1:0 --web-dir "$R$WEB" --allowed-host "$onion" \
		> "$CHECK_LOG" 2>&1 < /dev/null &
	pid=$!
	waited=0
	port=
	while [ -z "$port" ]; do
		[ "$waited" -lt 50 ] || { kill "$pid" 2> /dev/null || true; cat "$CHECK_LOG" >&2; die "the test instance didn't start"; }
		sleep 0.1 2> /dev/null || sleep 1
		waited=$((waited + 1))
		port=$(sed -n 's|.*http://127\.0\.0\.1:\([0-9][0-9]*\).*|\1|p' "$CHECK_LOG" 2> /dev/null)
	done
	echo "pid=$pid"
	echo "port=$port"
	echo "onion=$onion"
}

do_check_stop() {
	pid=${1:?check-stop needs a pid}
	case $pid in *[!0-9]*) die "bad pid '$pid'" ;; esac
	[ "$(cat "/proc/$pid/comm" 2> /dev/null)" = "$SERVICE" ] || die "pid $pid is not $SERVICE"
	memory "$pid"
	kill "$pid"
	rm -f "$CHECK_LOG"
}

do_print() {
	echo "### $INIT"
	init_script
	echo "### $CONF"
	server_config "<address>.onion"
	echo "### torrc block"
	printf '%s\n' "$BEGIN" "SocksPort 0 (only if torrc has no SocksPort of its own)" \
		"HiddenServiceDir $HS_DIR/" "HiddenServicePort 80 127.0.0.1:$PORT" "$END"
	echo "### $SYSUPGRADE block"
	printf '%s\n' "$BEGIN"
	sysupgrade_lines
	printf '%s\n' "$END"
}

command=${1:-}
[ $# -eq 0 ] || shift
case $command in
	install) do_install "$@" ;;
	uninstall) do_uninstall "$@" ;;
	status) do_status ;;
	check-start) do_check_start "$@" ;;
	check-stop) do_check_stop "$@" ;;
	print) do_print ;;
	*) die "usage: $0 install DIR | uninstall [--purge-keys] | status | check-start SETTINGS | check-stop PID | print" ;;
esac
