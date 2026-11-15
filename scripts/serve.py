#!/usr/bin/env python3
"""Serve the browser app and ephemeral three-player rooms.

A room is fixed at creation to one of two modes:

* ``webrtc``: the server forwards WebRTC signaling once per pairing;
* ``relay``: the server forwards opaque, end-to-end encrypted frames between
  seats (for Tor Browser, which has no WebRTC).

Every room event a player receives is kept in that player's acked log until
the player confirms it (``after=`` on reconnect, ``Last-Event-ID``, or ``ack``
in a relay POST). Tuning and fault-injection knobs are environment variables,
shared with the Rust server ``p2skat-room``; see ``Config``.
"""
import argparse
import json
import os
import random
import re
import secrets
import socket
import threading
import time
from collections import deque
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlsplit

WEB = Path(__file__).resolve().parents[1] / "web"
MAX_SIGNAL = 64 * 1024
MAX_BODY = MAX_SIGNAL * 2 + 4096
MAX_RELAY_BODY = 1024 * 1024 + 4096
MAX_PAYLOAD = 64 * 1024 + 64
MAX_SEQ = 2 ** 53 - 1
MAX_ROOMS = 64
RETRY_MS = 2000
PAYLOAD = re.compile(r"[A-Za-z0-9_-]+")
EVENT_ID = re.compile(r"[0-9]{1,16}")
CSP = ("default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; connect-src 'self'; "
       "img-src 'self' data:; style-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'")
CODES = {400: "bad_request", 401: "unauthorized", 403: "forbidden", 404: "not_found",
         409: "conflict", 413: "too_large", 415: "unsupported_media_type", 421: "wrong_host",
         422: "gap", 429: "over_limit", 503: "busy"}


def env_number(env, name, default, kind=float):
    raw = env.get(name, "")
    if raw == "":
        return default
    value = kind(raw)
    if value < 0:
        raise ValueError(f"{name} must not be negative")
    return value


class Config:
    """Tuning knobs. Defaults are the production values from the plan."""

    def __init__(self, env=os.environ):
        self.heartbeat = env_number(env, "P2SKAT_HEARTBEAT_MS", 10_000) / 1000
        self.liveness = env_number(env, "P2SKAT_LIVENESS_MS", 120_000) / 1000
        self.room_ttl = env_number(env, "P2SKAT_ROOM_TTL_MS", 3_600_000) / 1000
        self.closed_retain = env_number(env, "P2SKAT_CLOSED_RETAIN_MS", 30_000) / 1000
        self.rate = env_number(env, "P2SKAT_RATE", 50)
        self.burst = env_number(env, "P2SKAT_BURST", 50)
        self.log_events = env_number(env, "P2SKAT_LOG_EVENTS", 256, int)
        self.log_bytes = env_number(env, "P2SKAT_LOG_BYTES", 1 << 20, int)
        self.total_bytes = env_number(env, "P2SKAT_TOTAL_BYTES", 16 << 20, int)
        # Fault injection (tests only): "MEAN" or "MEAN,JITTER" in ms, and seconds between drops.
        latency = env.get("P2SKAT_FAULT_LATENCY_MS", "")
        mean, _, jitter = latency.partition(",")
        self.latency = float(mean) / 1000 if mean else 0.0
        self.jitter = (float(jitter) / 1000 if jitter else self.latency / 2) if mean else 0.0
        self.drop = env_number(env, "P2SKAT_FAULT_DROP_S", 0.0)

    def delay(self):
        if not self.latency:
            return 0.0
        return max(0.0, self.latency + random.uniform(-self.jitter, self.jitter))


class ApiError(Exception):
    def __init__(self, status, message, code=None, **extra):
        self.status, self.message, self.code, self.extra = status, message, code or CODES[status], extra


class OverLimit(Exception):
    pass


class Event:
    __slots__ = ("id", "data", "ready")

    def __init__(self, event_id, data, ready):
        self.id, self.data, self.ready = event_id, data, ready


class Player:
    def __init__(self, seat, name, browser, config):
        self.seat, self.name, self.browser = seat, name, browser
        self.token = secrets.token_urlsafe(24)
        self.log = deque()
        self.log_bytes = 0
        self.next_id = 1
        self.delivered = 0      # highest event id written to the current stream
        self.lost = 0           # highest event id discarded before the player acknowledged it
        self.generation = 0     # incremented for every new stream
        self.stream = 0         # generation of the attached stream, 0 if none
        self.detached_since = time.monotonic()
        self.drop_epoch = 0
        self.last_ready = 0.0
        self.tokens, self.refilled = float(config.burst), time.monotonic()
        self.sent = {}          # destination seat -> last relay seq accepted from this player

    def public(self):
        return {"seat": self.seat, "name": self.name, "browser": self.browser}


class Room:
    def __init__(self, mode):
        self.id = secrets.token_urlsafe(24)
        self.mode = mode
        self.players = []
        self.updated = time.monotonic()
        self.closed = False
        self.signaled = set()
        self.roster_version = 0

    def roster(self):
        return [player.public() for player in self.players]


class Server(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, address, *, config=None, max_rooms=MAX_ROOMS, web_dir=WEB, allowed_host=None):
        self.rooms = {}
        self.lock = threading.RLock()
        self.changed = threading.Condition(self.lock)
        self.config = config or Config()
        self.max_rooms, self.web_dir = max_rooms, str(web_dir)
        self.allowed_host = allowed_host.lower() if allowed_host else None
        self.total_bytes = 0
        self.stopping = threading.Event()
        super().__init__(address, Handler)
        if self.config.drop:
            threading.Thread(target=self.drop_loop, daemon=True).start()

    # Event logs -----------------------------------------------------------

    def discard(self, player):
        event = player.log.popleft()
        player.log_bytes -= len(event.data)
        self.total_bytes -= len(event.data)
        return event

    def acknowledge(self, player, upto):
        while player.log and player.log[0].id <= upto:
            self.discard(player)

    def append(self, player, event, *, limited=True):
        data = json.dumps(event, separators=(",", ":"))
        size = len(data)
        config = self.config

        def over():
            return (len(player.log) >= config.log_events or player.log_bytes + size > config.log_bytes or
                    self.total_bytes + size > config.total_bytes)

        if limited:
            # Events already written to a stream go first; undelivered ones never do.
            while over() and player.log and player.log[0].id <= player.delivered:
                player.lost = max(player.lost, self.discard(player).id)
            if len(player.log) >= config.log_events or player.log_bytes + size > config.log_bytes:
                raise OverLimit(f"{player.name}’s connection fell too far behind. Create a new table.")
            if self.total_bytes + size > config.total_bytes:
                raise OverLimit("The table server is out of message space. Create a new table.")
        now = time.monotonic()
        ready = max(now + config.delay(), player.last_ready) if config.latency else now
        player.last_ready = ready
        player.log.append(Event(player.next_id, data, ready))
        player.next_id += 1
        player.log_bytes += size
        self.total_bytes += size
        self.changed.notify_all()

    def close(self, room, message):
        if room.closed:
            return
        room.closed = True
        room.updated = time.monotonic()
        for player in room.players:
            while player.log:
                self.discard(player)
            # The terminal event bypasses every limit.
            self.append(player, {"type": "closed", "message": message}, limited=False)
        self.changed.notify_all()

    def delete(self, room_id):
        room = self.rooms.pop(room_id)
        for player in room.players:
            while player.log:
                self.discard(player)

    def cleanup(self):
        now = time.monotonic()
        config = self.config
        for room_id, room in list(self.rooms.items()):
            if room.closed:
                if now - room.updated > config.closed_retain:
                    self.delete(room_id)
                continue
            if now - room.updated > config.room_ttl:
                self.close(room, "This table expired. Create a new one.")
                continue
            if room.mode == "relay":
                for player in room.players:
                    if player.stream == 0 and now - player.detached_since > config.liveness:
                        self.close(room, f"{player.name} lost connection.")
                        break

    def service_actions(self):
        with self.lock:
            self.cleanup()

    def drop_loop(self):
        while not self.stopping.wait(self.config.drop):
            with self.lock:
                players = [player for room in self.rooms.values() if not room.closed
                           for player in room.players if player.stream]
                if players:
                    random.choice(players).drop_epoch += 1
                    self.changed.notify_all()

    def server_close(self):
        self.stopping.set()
        with self.lock:
            for room in self.rooms.values():
                self.close(room, "The table server stopped.")
        super().server_close()


def host_name(header):
    host = (header or "").lower()
    if host.startswith("["):
        return host[:host.find("]") + 1]
    return host.rsplit(":", 1)[0] if ":" in host else host


def parse_id(text):
    if text is None or text == "":
        return 0
    if not EVENT_ID.fullmatch(text):
        raise ApiError(400, "Invalid event id")
    return int(text)


def is_int(value):
    return type(value) is int


class Handler(SimpleHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    extensions_map = {**SimpleHTTPRequestHandler.extensions_map, ".js": "text/javascript",
                      ".mjs": "text/javascript", ".wasm": "application/wasm", ".css": "text/css",
                      ".html": "text/html", ".json": "application/json", ".svg": "image/svg+xml"}

    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=args[2].web_dir, **kwargs)

    def setup(self):
        super().setup()
        self.connection.settimeout(15)

    def log_message(self, *_args):
        # EventSource authenticates in its URL; never put query tokens in logs.
        pass

    def end_headers(self):
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("Referrer-Policy", "no-referrer")
        self.send_header("Content-Security-Policy", CSP)
        super().end_headers()

    def list_directory(self, _path):
        self.send_error(404)
        return None

    def response(self, status, data):
        payload = json.dumps(data, separators=(",", ":")).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def error_response(self, error):
        self.close_connection = True
        self.response(error.status, {"error": error.message, "code": error.code, **error.extra})

    def check_host(self):
        allowed = self.server.allowed_host
        if allowed and host_name(self.headers.get("Host")) != allowed:
            raise ApiError(421, "Unknown host")

    def origin(self):
        origin = self.headers.get("Origin")
        if origin:
            parsed = urlsplit(origin)
            if parsed.scheme not in ("http", "https") or parsed.netloc != self.headers.get("Host") or parsed.path or parsed.query or parsed.fragment:
                raise ApiError(403, "Requests must come from this website")
        if self.headers.get("Sec-Fetch-Site") == "cross-site":
            raise ApiError(403, "Requests must come from this website")

    def body(self, limit):
        if self.headers.get_content_type() != "application/json":
            raise ApiError(415, "Send JSON")
        try:
            length = int(self.headers.get("Content-Length", "-1"))
            if not 0 <= length <= limit:
                raise ApiError(413, "Request is too large")
            data = json.loads(self.rfile.read(length))
        except (ValueError, UnicodeDecodeError):
            raise ApiError(400, "Invalid JSON") from None
        if not isinstance(data, dict):
            raise ApiError(400, "Expected a JSON object")
        return data

    def room(self, room_id, *, allow_closed=False):
        self.server.cleanup()
        room = self.server.rooms.get(room_id)
        if room is None:
            raise ApiError(404, "This table ended or expired")
        if room.closed and not allow_closed:
            raise ApiError(404, "This table ended or expired", "room_closed")
        return room

    def player(self, room, token):
        if not isinstance(token, str) or len(token) > 128 or not token.isascii():
            raise ApiError(401, "Invalid player token")
        for player in room.players:
            if secrets.compare_digest(player.token, token):
                return player
        raise ApiError(401, "Invalid player token")

    def bearer(self):
        header = self.headers.get("Authorization", "")
        return header[7:] if header.startswith("Bearer ") else ""

    def join(self, room, body):
        if not {"name", "browser"} <= set(body) <= {"name", "browser", "mode"}:
            raise ApiError(400, "Supply your name and browser identifier")
        name, browser = body["name"], body["browser"]
        if not isinstance(name, str) or not 1 <= len(name.strip()) <= 40 or not name.isprintable():
            raise ApiError(400, "Names must have 1–40 readable characters")
        if not isinstance(browser, str) or not 1 <= len(browser) <= 48 or not all(char.isascii() and (char.isalnum() or char in "-_") for char in browser):
            raise ApiError(400, "Invalid browser identifier")
        if "mode" in body:
            if body["mode"] not in ("webrtc", "relay"):
                raise ApiError(400, "Mode must be webrtc or relay")
            if body["mode"] != room.mode:
                raise ApiError(400, f"This table uses {room.mode} mode", "wrong_mode")
        if len(room.players) == 3:
            raise ApiError(409, "This table already has three players", "room_full")
        player = Player(len(room.players), name.strip(), browser, self.server.config)
        room.players.append(player)
        room.updated = time.monotonic()
        room.roster_version += 1
        self.server.changed.notify_all()
        return {"room": room.id, "seat": player.seat, "token": player.token, "players": room.roster(), "mode": room.mode}

    def emit(self, room, player, event):
        try:
            self.server.append(player, event)
        except OverLimit as error:
            self.server.close(room, str(error))
            raise ApiError(429, str(error)) from None

    def signal(self, room, player, body):
        if room.mode != "webrtc":
            raise ApiError(400, "This table relays messages; it does not use WebRTC", "wrong_mode")
        if set(body) != {"to", "signal"}:
            raise ApiError(400, "Supply a destination and connection signal")
        to, text = body["to"], body["signal"]
        if not is_int(to) or not 0 <= to < len(room.players) or to == player.seat:
            raise ApiError(400, "Invalid destination seat")
        if not isinstance(text, str):
            raise ApiError(400, "Invalid connection signal")
        try:
            size = len(text.encode("utf-8"))
        except UnicodeEncodeError:
            raise ApiError(400, "Invalid connection signal") from None
        if size > MAX_SIGNAL:
            raise ApiError(413, "Connection signal is too large")
        try:
            signal = json.loads(text)
        except ValueError:
            raise ApiError(400, "Invalid connection signal") from None
        if (len(room.players) != 3 or not isinstance(signal, dict) or
                set(signal) != {"version", "from", "to", "names", "type", "sdp"} or
                not is_int(signal["version"]) or signal["version"] != 1 or
                not is_int(signal["from"]) or signal["from"] != player.seat or
                not is_int(signal["to"]) or signal["to"] != to or
                signal["names"] != [other.name for other in room.players] or
                signal["type"] != ("offer" if player.seat < to else "answer") or
                not isinstance(signal["sdp"], str) or not signal["sdp"].startswith("v=0\r\n")):
            raise ApiError(400, "Connection details do not match this table or sender")
        if (player.seat, to) in room.signaled:
            raise ApiError(409, "This pairing already exchanged connection details", "duplicate_signal")
        self.emit(room, room.players[to], {"type": "signal", "from": player.seat, "signal": text})
        room.signaled.add((player.seat, to))
        room.updated = time.monotonic()
        return {"ok": True}

    def relay(self, room, player, body):
        if room.mode != "relay":
            raise ApiError(400, "This table uses WebRTC; it does not relay messages", "wrong_mode")
        if "frames" not in body or not set(body) <= {"frames", "ack"} or not isinstance(body["frames"], list):
            raise ApiError(400, "Supply a list of frames")
        ack = body.get("ack", 0)
        if not is_int(ack) or not 0 <= ack < player.next_id:
            raise ApiError(400, "Invalid acknowledgement")
        frames = body["frames"]
        for frame in frames:
            if not isinstance(frame, dict) or set(frame) != {"to", "seq", "payload"}:
                raise ApiError(400, "Frames need exactly to, seq and payload")
            to, seq, payload = frame["to"], frame["seq"], frame["payload"]
            if not is_int(to) or not 0 <= to < len(room.players) or to == player.seat:
                raise ApiError(400, "Invalid destination seat")
            if not is_int(seq) or not 1 <= seq <= MAX_SEQ:
                raise ApiError(400, "Invalid sequence number")
            if not isinstance(payload, str) or not PAYLOAD.fullmatch(payload) or len(payload) % 4 == 1:
                raise ApiError(400, "Payloads must be unpadded base64url")
            if len(payload) * 3 // 4 > MAX_PAYLOAD:
                raise ApiError(413, "Frame payload is too large")
        now = time.monotonic()
        config = self.server.config
        player.tokens = min(float(config.burst), player.tokens + (now - player.refilled) * config.rate)
        player.refilled = now
        if len(frames) > player.tokens:
            message = f"{player.name} sent messages too quickly. Create a new table."
            self.server.close(room, message)
            raise ApiError(429, message)
        player.tokens -= len(frames)
        expected, deliver = dict(player.sent), []
        for frame in frames:
            to, seq = frame["to"], frame["seq"]
            last = expected.get(to, 0)
            if seq == last + 1:
                expected[to] = seq
                deliver.append(frame)
            elif seq > last + 1:
                raise ApiError(422, "A frame is missing before this one", expected=self.expected(room, player))
        self.server.acknowledge(player, ack)
        for frame in deliver:
            self.emit(room, room.players[frame["to"]],
                      {"type": "relay", "from": player.seat, "seq": frame["seq"], "payload": frame["payload"]})
        player.sent = expected
        room.updated = now
        return {"expected": self.expected(room, player)}

    @staticmethod
    def expected(room, player):
        return {str(other.seat): player.sent.get(other.seat, 0) + 1 for other in room.players if other is not player}

    def latency(self, player=None, epoch=None):
        """Fault injection: hold the response; return False if the connection should be dropped."""
        delay = self.server.config.delay()
        if not delay:
            return True
        deadline = time.monotonic() + delay
        with self.server.changed:
            while True:
                if player is not None and player.drop_epoch != epoch:
                    return False
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    return True
                self.server.changed.wait(remaining)

    def drop_connection(self):
        self.close_connection = True
        try:
            self.connection.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass

    def do_POST(self):
        player = epoch = None
        try:
            self.check_host()
            self.origin()
            parts = urlsplit(self.path).path.strip("/").split("/")
            body = self.body(MAX_RELAY_BODY if parts[-1:] == ["relay"] else MAX_BODY)
            with self.server.lock:
                if parts == ["api", "rooms"]:
                    if not {"name", "browser"} <= set(body) <= {"name", "browser", "mode"}:
                        raise ApiError(400, "Supply your name and browser identifier")
                    mode = body.get("mode", "webrtc")
                    if mode not in ("webrtc", "relay"):
                        raise ApiError(400, "Mode must be webrtc or relay")
                    self.server.cleanup()
                    if len(self.server.rooms) >= self.server.max_rooms:
                        raise ApiError(503, "Too many tables. Try again later.")
                    room = Room(mode)
                    result = self.join(room, body)
                    self.server.rooms[room.id] = room
                elif len(parts) == 4 and parts[:2] == ["api", "rooms"]:
                    room = self.room(parts[2])
                    if parts[3] == "join":
                        result = self.join(room, body)
                    else:
                        player = self.player(room, self.bearer())
                        epoch = player.drop_epoch
                        if parts[3] == "signal":
                            result = self.signal(room, player, body)
                        elif parts[3] == "relay":
                            result = self.relay(room, player, body)
                        elif parts[3] == "leave":
                            if body:
                                raise ApiError(400, "Leave does not accept a payload")
                            self.server.close(room, f"{player.name} left the table.")
                            result = {"ok": True}
                        else:
                            raise ApiError(404, "Unknown endpoint")
                else:
                    raise ApiError(404, "Unknown endpoint")
            if not self.latency(player, epoch):
                self.drop_connection()
                return
            self.response(200, result)
        except ApiError as error:
            if self.latency(player, epoch):
                self.error_response(error)
            else:
                self.drop_connection()
        except (OSError, ValueError):
            self.close_connection = True

    def do_HEAD(self):
        try:
            self.check_host()
        except ApiError as error:
            self.error_response(error)
            return
        super().do_HEAD()

    def do_GET(self):
        parsed = urlsplit(self.path)
        try:
            self.check_host()
            if parsed.path == "/api/health":
                self.response(200, {"ok": True})
                return
            if not parsed.path.startswith("/api/"):
                super().do_GET()
                return
            parts = parsed.path.strip("/").split("/")
            if len(parts) != 4 or parts[:2] != ["api", "rooms"] or parts[3] != "events":
                raise ApiError(404, "Unknown endpoint")
            query = parse_qs(parsed.query)
            after = max(parse_id(query.get("after", [""])[0]), parse_id(self.headers.get("Last-Event-ID")))
            with self.server.lock:
                room = self.room(parts[2], allow_closed=True)
                player = self.player(room, query.get("token", [""])[0])
                if after >= player.next_id:
                    raise ApiError(400, "Invalid event id")
                if not room.closed and after < player.lost:
                    self.server.close(room, f"{player.name} missed messages after a connection problem.")
                if room.closed:
                    # Replay only the terminal event.
                    cursor, generation = player.log[-1].id - 1, None
                else:
                    self.server.acknowledge(player, after)
                    player.delivered = cursor = after
                    player.generation += 1
                    generation = player.stream = player.generation
                    player.detached_since = None
                    self.server.changed.notify_all()
            self.events(room, player, cursor, generation)
        except ApiError as error:
            self.error_response(error)

    def events(self, room, player, cursor, generation):
        server, config = self.server, self.server.config
        self.close_connection = True
        roster_sent = -1 if generation else room.roster_version
        epoch = player.drop_epoch
        try:
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Connection", "close")
            self.send_header("X-Accel-Buffering", "no")
            self.end_headers()
            self.wfile.write(f"retry: {RETRY_MS}\n\n".encode())
            last_write = time.monotonic()
            while True:
                chunks, finished = [], False
                with server.changed:
                    while True:
                        if server.stopping.is_set():
                            return
                        if generation and (player.stream != generation or player.drop_epoch != epoch):
                            if player.drop_epoch != epoch:
                                self.drop_connection()
                            return
                        now = time.monotonic()
                        if not room.closed and room.roster_version != roster_sent:
                            roster_sent = room.roster_version
                            chunks.append("data: " + json.dumps({"type": "players", "players": room.roster()}, separators=(",", ":")))
                        wake = last_write + config.heartbeat
                        for event in player.log:
                            if event.id <= cursor:
                                continue
                            if event.ready > now:
                                wake = min(wake, event.ready)
                                break
                            chunks.append(f"id: {event.id}\ndata: {event.data}")
                            cursor = event.id
                            if event.data.startswith('{"type":"closed"'):
                                finished = True
                                break
                        if chunks:
                            break
                        if now >= last_write + config.heartbeat:
                            chunks.append('event: ping\ndata: {"type":"ping"}')
                            break
                        server.changed.wait(max(0.005, wake - now))
                self.wfile.write("".join(chunk + "\n\n" for chunk in chunks).encode())
                self.wfile.flush()
                last_write = time.monotonic()
                with server.lock:
                    if generation and player.stream == generation:
                        player.delivered = max(player.delivered, cursor)
                    if not room.closed:
                        room.updated = last_write
                if finished:
                    return
        except OSError:
            pass
        finally:
            with server.lock:
                if generation and player.stream == generation:
                    player.stream = 0
                    player.detached_since = time.monotonic()


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8080)
    parser.add_argument("--max-rooms", type=int, default=MAX_ROOMS)
    parser.add_argument("--web-dir", default=str(WEB))
    parser.add_argument("--allowed-host", help="only answer requests whose Host is this name")
    args = parser.parse_args()
    with Server((args.host, args.port), max_rooms=args.max_rooms, web_dir=args.web_dir,
                allowed_host=args.allowed_host) as server:
        print(f"p2skat: http://{args.host}:{server.server_port}", flush=True)
        try:
            server.serve_forever(poll_interval=0.1)
        except KeyboardInterrupt:
            pass


if __name__ == "__main__":
    main()
