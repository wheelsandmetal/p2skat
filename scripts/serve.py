#!/usr/bin/env python3
"""Serve the browser app and ephemeral, three-player signaling rooms."""
import argparse
import json
import queue
import secrets
import threading
import time
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlsplit

WEB = Path(__file__).resolve().parents[1] / "web"
MAX_SIGNAL = 64 * 1024
ROOM_TTL = 60 * 60
MAX_ROOMS = 64
HEARTBEAT = 10


class ApiError(Exception):
    def __init__(self, status, message):
        self.status, self.message = status, message


class Player:
    def __init__(self, seat, name, browser):
        self.seat, self.name, self.browser = seat, name, browser
        self.token = secrets.token_urlsafe(24)
        self.events = queue.Queue(maxsize=16)
        self.streaming = False

    def public(self):
        return {"seat": self.seat, "name": self.name, "browser": self.browser}


class Room:
    def __init__(self):
        self.id = secrets.token_urlsafe(24)
        self.players = []
        self.updated = time.monotonic()
        self.closed = False
        self.signaled = set()

    def roster(self):
        return [player.public() for player in self.players]

    def close(self, message):
        if self.closed:
            return
        self.closed = True
        self.updated = time.monotonic()
        for player in self.players:
            # A terminal event must survive a full signaling queue.
            while not player.events.empty():
                try:
                    player.events.get_nowait()
                except queue.Empty:
                    break
            player.events.put_nowait({"type": "closed", "message": message})

    def emit(self, player, event):
        try:
            player.events.put_nowait(event)
        except queue.Full:
            self.close("Too many pending connection messages. Create a new table.")
            raise ApiError(409, "The table's connection queue is full")


class Server(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, address, *, ttl=ROOM_TTL, max_rooms=MAX_ROOMS):
        self.rooms = {}
        self.lock = threading.RLock()
        self.ttl, self.max_rooms = ttl, max_rooms
        self.stopping = threading.Event()
        super().__init__(address, Handler)

    def cleanup(self):
        now = time.monotonic()
        for room_id, room in list(self.rooms.items()):
            if now - room.updated > (30 if room.closed else self.ttl):
                room.close("This table expired. Create a new one.")
                del self.rooms[room_id]

    def service_actions(self):
        with self.lock:
            self.cleanup()

    def server_close(self):
        self.stopping.set()
        with self.lock:
            for room in self.rooms.values():
                room.close("The signaling server stopped.")
        super().server_close()


class Handler(SimpleHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=str(WEB), **kwargs)

    def setup(self):
        super().setup()
        self.connection.settimeout(15)

    def log_message(self, *_args):
        # EventSource authenticates in its URL; never put query tokens in logs.
        pass

    def end_headers(self):
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Content-Type-Options", "nosniff")
        super().end_headers()

    def response(self, status, data):
        payload = json.dumps(data, separators=(",", ":")).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def origin(self):
        origin = self.headers.get("Origin")
        if origin:
            parsed = urlsplit(origin)
            if parsed.scheme not in ("http", "https") or parsed.netloc != self.headers.get("Host") or parsed.path or parsed.query or parsed.fragment:
                raise ApiError(403, "Requests must come from this website")
        if self.headers.get("Sec-Fetch-Site") == "cross-site":
            raise ApiError(403, "Requests must come from this website")

    def body(self):
        if self.headers.get_content_type() != "application/json":
            raise ApiError(415, "Send JSON")
        try:
            length = int(self.headers.get("Content-Length", "-1"))
            if not 0 <= length <= MAX_SIGNAL * 2 + 4096:
                raise ApiError(413, "Request is too large")
            data = json.loads(self.rfile.read(length))
        except (ValueError, UnicodeDecodeError):
            raise ApiError(400, "Invalid JSON") from None
        if not isinstance(data, dict):
            raise ApiError(400, "Expected a JSON object")
        return data

    def room(self, room_id):
        self.server.cleanup()
        room = self.server.rooms.get(room_id)
        if room is None or room.closed:
            raise ApiError(404, "This table ended or expired")
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
        if set(body) != {"name", "browser"}:
            raise ApiError(400, "Supply your name and browser identifier")
        name, browser = body["name"], body["browser"]
        if not isinstance(name, str) or not 1 <= len(name.strip()) <= 40 or not name.isprintable():
            raise ApiError(400, "Names must have 1–40 readable characters")
        if not isinstance(browser, str) or not 1 <= len(browser) <= 48 or not all(char.isascii() and (char.isalnum() or char in "-_") for char in browser):
            raise ApiError(400, "Invalid browser identifier")
        if len(room.players) == 3:
            raise ApiError(409, "This table already has three players")
        player = Player(len(room.players), name.strip(), browser)
        room.players.append(player)
        room.updated = time.monotonic()
        for other in room.players:
            if other.streaming:
                room.emit(other, {"type": "players", "players": room.roster()})
        return {"room": room.id, "seat": player.seat, "token": player.token, "players": room.roster()}

    def signal(self, room, player, body):
        if set(body) != {"to", "signal"}:
            raise ApiError(400, "Supply a destination and connection signal")
        to, text = body["to"], body["signal"]
        if type(to) is not int or not 0 <= to < len(room.players) or to == player.seat:
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
                type(signal["version"]) is not int or signal["version"] != 1 or
                type(signal["from"]) is not int or signal["from"] != player.seat or
                type(signal["to"]) is not int or signal["to"] != to or
                signal["names"] != [other.name for other in room.players] or
                signal["type"] != ("offer" if player.seat < to else "answer") or
                not isinstance(signal["sdp"], str) or not signal["sdp"].startswith("v=0\r\n")):
            raise ApiError(400, "Connection details do not match this table or sender")
        if (player.seat, to) in room.signaled:
            raise ApiError(409, "This pairing already exchanged connection details")
        room.emit(room.players[to], {"type": "signal", "from": player.seat, "signal": text})
        room.signaled.add((player.seat, to))
        room.updated = time.monotonic()

    def do_POST(self):
        try:
            self.origin()
            body = self.body()
            parts = urlsplit(self.path).path.strip("/").split("/")
            with self.server.lock:
                if parts == ["api", "rooms"]:
                    self.server.cleanup()
                    if len(self.server.rooms) >= self.server.max_rooms:
                        raise ApiError(503, "Too many tables. Try again later.")
                    room = Room()
                    result = self.join(room, body)
                    self.server.rooms[room.id] = room
                elif len(parts) == 4 and parts[:2] == ["api", "rooms"]:
                    room = self.room(parts[2])
                    if parts[3] == "join":
                        result = self.join(room, body)
                    else:
                        player = self.player(room, self.bearer())
                        if parts[3] == "signal":
                            self.signal(room, player, body)
                        elif parts[3] == "leave":
                            if body:
                                raise ApiError(400, "Leave does not accept a payload")
                            room.close(f"{player.name} left the table.")
                        else:
                            raise ApiError(404, "Unknown endpoint")
                        result = {"ok": True}
                else:
                    raise ApiError(404, "Unknown endpoint")
            self.response(200, result)
        except ApiError as error:
            self.close_connection = True
            self.response(error.status, {"error": error.message})
        except (OSError, ValueError):
            self.close_connection = True

    def do_GET(self):
        parsed = urlsplit(self.path)
        if parsed.path == "/api/health":
            self.response(200, {"ok": True})
            return
        if not parsed.path.startswith("/api/"):
            super().do_GET()
            return
        try:
            parts = parsed.path.strip("/").split("/")
            if len(parts) != 4 or parts[:2] != ["api", "rooms"] or parts[3] != "events":
                raise ApiError(404, "Unknown endpoint")
            with self.server.lock:
                room = self.room(parts[2])
                player = self.player(room, parse_qs(parsed.query).get("token", [""])[0])
                if player.streaming:
                    raise ApiError(409, "This player already has an event connection")
                player.streaming = True
                initial = {"type": "players", "players": room.roster()}
            self.events(room, player, initial)
        except ApiError as error:
            self.response(error.status, {"error": error.message})

    def events(self, room, player, initial):
        self.close_connection = True
        try:
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Connection", "close")
            self.send_header("X-Accel-Buffering", "no")
            self.end_headers()
            event = initial
            while not self.server.stopping.is_set():
                if event is None:
                    self.wfile.write(b": heartbeat\n\n")
                else:
                    self.wfile.write(("data: " + json.dumps(event, separators=(",", ":")) + "\n\n").encode())
                self.wfile.flush()
                if event is not None and event["type"] == "closed":
                    break
                with self.server.lock:
                    if not room.closed:
                        room.updated = time.monotonic()
                try:
                    event = player.events.get(timeout=HEARTBEAT)
                except queue.Empty:
                    event = None
        except OSError:
            pass
        finally:
            with self.server.lock:
                player.streaming = False


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8080)
    args = parser.parse_args()
    with Server((args.host, args.port)) as server:
        print(f"p2skat: http://{args.host}:{server.server_port}", flush=True)
        try:
            server.serve_forever()
        except KeyboardInterrupt:
            pass


if __name__ == "__main__":
    main()
