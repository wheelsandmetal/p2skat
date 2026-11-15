"""Black-box conformance suite for the p2skat room servers (serve.py and p2skat-room).

Every test talks plain HTTP and SSE to a server process:

  python3 scripts/test_serve.py                      # starts scripts/serve.py for each test
  P2SKAT_SERVER=rust python3 scripts/test_serve.py   # starts target/debug/p2skat-room
                                                     # (override with P2SKAT_ROOM_BIN=/path/to/p2skat-room)
  P2SKAT_SERVER_URL=http://127.0.0.1:8080 python3 scripts/test_serve.py

With P2SKAT_SERVER_URL the suite uses an already running server. Tests that
need a special configuration are skipped. Tests that need short timings run
only if the server was started with the environment printed by
`python3 scripts/test_serve.py --print-env` and P2SKAT_SERVER_FAST=1 is set.
P2SKAT_SERVER_HOST=<name> sends that Host header (and the matching Origin), for
a server started with --allowed-host and reached through a tunnel, as
`scripts/deploy-router.sh --check` does.
"""
import base64
import concurrent.futures
import http.client
import json
import os
import re
import socket
import contextlib
import subprocess
import sys
import threading
import time
import unittest
from pathlib import Path
from urllib.parse import urlsplit

ROOT = Path(__file__).resolve().parents[1]
MAX_SIGNAL = 64 * 1024
MAX_PAYLOAD = 64 * 1024 + 64
CSP = ("default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; connect-src 'self'; "
       "img-src 'self' data:; style-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'")
# Short timings so liveness and heartbeat behaviour is testable in seconds.
FAST_ENV = {"P2SKAT_HEARTBEAT_MS": "300", "P2SKAT_LIVENESS_MS": "1500"}
NAMES = ["Ada", "Bert", "Clara"]


def server_command(port_args):
    kind = os.environ.get("P2SKAT_SERVER", "python")
    if kind in ("python", "serve.py"):
        return [sys.executable, str(ROOT / "scripts" / "serve.py"), "--port", "0", *port_args]
    if kind == "rust":
        binary = os.environ.get("P2SKAT_ROOM_BIN", str(ROOT / "target" / "debug" / "p2skat-room"))
        return [binary, "--bind", "127.0.0.1:0", "--web-dir", str(ROOT / "web"), *port_args]
    raise SystemExit(f"Unknown P2SKAT_SERVER={kind!r}; use python or rust")


class ServerProcess:
    def __init__(self, env, args):
        environment = {key: value for key, value in os.environ.items() if not key.startswith("P2SKAT_") or key in ("P2SKAT_SERVER", "P2SKAT_ROOM_BIN")}
        environment.update(FAST_ENV)
        environment.update(env)
        self.process = subprocess.Popen(server_command(list(args)), env=environment, stdout=subprocess.PIPE,
                                        stderr=subprocess.DEVNULL, text=True)
        line = self.process.stdout.readline()
        match = re.search(r"http://([^\s/]+):(\d+)", line)
        if not match:
            self.stop()
            raise RuntimeError(f"Server did not report its address: {line!r}")
        self.host, self.port = match.group(1), int(match.group(2))

    def stop(self):
        self.process.terminate()
        try:
            self.process.wait(5)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()
        self.process.stdout.close()


def server(env=None, args=(), fast=False):
    """Run this test against a server with extra environment and arguments."""
    def decorate(test):
        test.server_env, test.server_args, test.server_fast = env or {}, tuple(args), fast
        return test
    return decorate


def payload(size, byte=b"x"):
    return base64.urlsafe_b64encode(byte * size).rstrip(b"=").decode()


class Stream:
    """A minimal SSE reader that copes with chunked and close-delimited bodies."""

    def __init__(self, connection, sock, response):
        self.connection, self.sock, self.response = connection, sock, response
        self.retry = None
        self.eof = False

    def next(self, timeout=3.0, pings=False):
        """Return the next event as {id, event, data}, None at EOF or after the timeout."""
        deadline = time.monotonic() + timeout
        fields = {}
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return None
            self.sock.settimeout(remaining)
            try:
                line = self.response.readline()
            except (TimeoutError, socket.timeout):
                return None
            except (OSError, http.client.HTTPException):
                line = b""
            if not line:
                self.eof = True
                return None
            line = line.decode().rstrip("\r\n")
            if line == "":
                if "data" in fields:
                    event = {"id": int(fields["id"]) if "id" in fields else None,
                             "event": fields.get("event", "message"), "data": json.loads(fields["data"])}
                    if event["event"] == "ping" and not pings:
                        fields = {}
                        continue
                    return event
                fields = {}
                continue
            if line.startswith(":"):
                continue
            name, _, value = line.partition(":")
            value = value[1:] if value.startswith(" ") else value
            if name == "retry":
                self.retry = int(value)
            elif name == "data":
                fields["data"] = fields["data"] + "\n" + value if "data" in fields else value
            else:
                fields[name] = value

    def data(self, timeout=3.0):
        event = self.next(timeout)
        if event is None:
            raise AssertionError("SSE ended or timed out without an event")
        return event["data"]

    def closed_event(self, timeout=3.0):
        """Skip events until the terminal closed event and return its data."""
        deadline = time.monotonic() + timeout
        while True:
            event = self.next(deadline - time.monotonic())
            if event is None:
                raise AssertionError("SSE ended or timed out without a closed event")
            if event["data"]["type"] == "closed":
                return event["data"]

    def ended(self, timeout=3.0):
        """True if the stream reaches EOF within the timeout (ignoring events)."""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self.next(deadline - time.monotonic(), pings=True) is None and self.eof:
                return True
        return self.eof

    def close(self):
        self.response.close()
        self.connection.close()


class ConformanceTest(unittest.TestCase):
    def setUp(self):
        test = getattr(self, self._testMethodName)
        env, args, fast = (getattr(test, "server_env", {}), getattr(test, "server_args", ()),
                           getattr(test, "server_fast", False))
        self.process = None
        self.host_header = None
        external = os.environ.get("P2SKAT_SERVER_URL")
        if external:
            if env or args:
                self.skipTest("needs a specially configured server")
            if fast and os.environ.get("P2SKAT_SERVER_FAST") != "1":
                self.skipTest("needs short timings (P2SKAT_SERVER_FAST=1)")
            parsed = urlsplit(external)
            self.host, self.port = parsed.hostname, parsed.port or 80
            self.host_header = os.environ.get("P2SKAT_SERVER_HOST") or None
        else:
            self.process = ServerProcess(env, args)
            self.host, self.port = self.process.host, self.process.port
        self.origin = f"http://{self.host_header or f'{self.host}:{self.port}'}"
        self.streams = []

    def tearDown(self):
        for stream in self.streams:
            stream.close()
        if self.process:
            self.process.stop()

    # HTTP helpers ------------------------------------------------------------

    def connection(self, timeout=5):
        return http.client.HTTPConnection(self.host, self.port, timeout=timeout)

    def raw(self, method, path, body=None, headers=None):
        connection = self.connection()
        headers = dict(headers or {})
        if self.host_header and "Host" not in headers:
            headers["Host"] = self.host_header
        connection.request(method, path, body, headers)
        response = connection.getresponse()
        data = response.read()
        connection.close()
        return response, data

    def request(self, path, body=None, token=None, origin=None, method="POST", headers=None):
        all_headers = {"Content-Type": "application/json", "Origin": origin or self.origin}
        if token:
            all_headers["Authorization"] = f"Bearer {token}"
        all_headers.update(headers or {})
        response, data = self.raw(method, path, None if body is None else json.dumps(body), all_headers)
        return response.status, json.loads(data)

    def table(self, mode=None, names=NAMES):
        body = {"name": names[0], "browser": "browser0"}
        if mode:
            body["mode"] = mode
        status, host = self.request("/api/rooms", body)
        self.assertEqual(status, 200, host)
        players = [host]
        for index, name in enumerate(names[1:], 1):
            status, player = self.request(f"/api/rooms/{host['room']}/join", {"name": name, "browser": f"browser{index}"})
            self.assertEqual(status, 200, player)
            players.append(player)
        return host["room"], players

    def stream(self, room, player, after=None, last_event_id=None, status=200):
        connection = self.connection()
        connection.connect()
        sock = connection.sock
        path = f"/api/rooms/{room}/events?token={player['token']}"
        if after is not None:
            path += f"&after={after}"
        connection.putrequest("GET", path, skip_host=bool(self.host_header))
        if self.host_header:
            connection.putheader("Host", self.host_header)
        if last_event_id is not None:
            connection.putheader("Last-Event-ID", str(last_event_id))
        connection.endheaders()
        response = connection.getresponse()
        self.assertEqual(response.status, status)
        if status != 200:
            data = json.loads(response.read())
            connection.close()
            return data
        self.assertEqual(response.getheader("Content-Type"), "text/event-stream")
        stream = Stream(connection, sock, response)
        self.streams.append(stream)
        return stream

    def signal(self, source=0, destination=1, **changes):
        signal = {"version": 1, "from": source, "to": destination, "names": NAMES,
                  "type": "offer" if source < destination else "answer", "sdp": "v=0\r\ns=test\r\n"}
        signal.update(changes)
        return {"to": destination, "signal": json.dumps(signal)}

    def relay(self, room, player, frames, **extra):
        return self.request(f"/api/rooms/{room}/relay", {"frames": frames, **extra}, player["token"])

    @contextlib.contextmanager
    def keepalive(self, room, players, every=0.3):
        """Ack-only relay POSTs from these players in the background, like the browser client."""
        stop = threading.Event()
        statuses = []

        def run():
            while not stop.wait(every):
                for player in players:
                    statuses.append(self.relay(room, player, [])[0])

        thread = threading.Thread(target=run, daemon=True)
        thread.start()
        try:
            yield statuses
        finally:
            stop.set()
            thread.join()

    def raw_post_headers(self, path, length, token=None):
        """Send only the headers of a JSON POST that announces `length` body bytes."""
        sock = socket.create_connection((self.host, self.port), timeout=5)
        host = self.host_header or f"{self.host}:{self.port}"
        lines = [f"POST {path} HTTP/1.1", f"Host: {host}", f"Origin: {self.origin}",
                 "Content-Type: application/json", f"Content-Length: {length}"]
        if token:
            lines.append(f"Authorization: Bearer {token}")
        sock.sendall(("\r\n".join(lines) + "\r\n\r\n").encode())
        return sock

    def expect_error(self, result, status, code):
        self.assertEqual(result[0], status, result[1])
        self.assertEqual(result[1].get("code"), code, result[1])
        self.assertIsInstance(result[1].get("error"), str)


class LobbyTests(ConformanceTest):
    def test_join_assigns_atomic_fixed_seats_and_capacity(self):
        status, host = self.request("/api/rooms", {"name": "Ada", "browser": "same-browser"})
        self.assertEqual(status, 200)
        self.assertEqual(host["mode"], "webrtc")
        path = f"/api/rooms/{host['room']}/join"
        with concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:
            results = list(pool.map(lambda index: self.request(path, {"name": f"Player{index}", "browser": "same-browser"}), range(3)))
        successful = [player for status, player in results if status == 200]
        self.assertEqual(sorted(player["seat"] for player in successful), [1, 2])
        self.assertEqual([player["code"] for status, player in results if status != 200], ["room_full"])
        self.assertEqual(sum(status == 409 for status, _ in results), 1)
        self.assertEqual(len({host["token"], *(player["token"] for player in successful)}), 3)
        self.assertTrue(all("token" not in player for player in successful[-1]["players"]))

    def test_sse_roster_forwarding_and_leave(self):
        room, players = self.table()
        stream = self.stream(room, players[1])
        self.assertEqual(stream.next(), {"id": None, "event": "message", "data": {"type": "players", "players": players[2]["players"]}})
        self.assertEqual(stream.retry, 2000)
        payload = self.signal()
        self.assertEqual(self.request(f"/api/rooms/{room}/signal", payload, players[0]["token"]), (200, {"ok": True}))
        event = stream.next()
        self.assertEqual(event["data"], {"type": "signal", "from": 0, "signal": payload["signal"]})
        self.assertEqual(event["id"], 1)
        self.assertEqual(self.request(f"/api/rooms/{room}/leave", {}, players[0]["token"]), (200, {"ok": True}))
        closed = stream.next()
        self.assertEqual(closed["data"], {"type": "closed", "message": "Ada left the table."})
        self.assertEqual(closed["id"], 2)
        self.assertTrue(stream.ended())
        self.expect_error(self.request(f"/api/rooms/{room}/signal", self.signal(1, 2), players[1]["token"]), 404, "room_closed")
        # A reconnect to a recently closed table still learns why it closed.
        late = self.stream(room, players[2])
        self.assertEqual(late.data()["type"], "closed")
        self.assertTrue(late.ended())

    def test_sse_roster_updates_and_stream_supersede(self):
        _, host = self.request("/api/rooms", {"name": "Ada", "browser": "browser0"})
        room = host["room"]
        first = self.stream(room, host)
        self.assertEqual(len(first.data()["players"]), 1)
        second = self.stream(room, host)
        self.assertTrue(first.ended(), "A new stream must replace the old one")
        self.assertEqual(len(second.data()["players"]), 1)
        for index, name in enumerate(("Bert", "Clara"), 1):
            self.assertEqual(self.request(f"/api/rooms/{room}/join", {"name": name, "browser": f"browser{index}"})[0], 200)
            event = second.next()
            self.assertIsNone(event["id"], "Roster snapshots are not logged")
            self.assertEqual(len(event["data"]["players"]), index + 1)

    def test_signal_retained_before_sse_connects(self):
        room, players = self.table()
        payload = self.signal()
        self.assertEqual(self.request(f"/api/rooms/{room}/signal", payload, players[0]["token"])[0], 200)
        stream = self.stream(room, players[1])
        self.assertEqual(len(stream.data()["players"]), 3)
        self.assertEqual(stream.data()["signal"], payload["signal"])

    def test_sender_payload_and_origin_validation(self):
        room, players = self.table()
        path = f"/api/rooms/{room}/signal"
        self.expect_error(self.request(path, self.signal(), "forged-token"), 401, "unauthorized")
        self.assertEqual(self.request(path, self.signal(), "ÿ")[0], 401)
        self.expect_error(self.request(path, self.signal(), players[0]["token"], origin="https://other.example"), 403, "forbidden")
        self.assertEqual(self.request(path, self.signal(), players[0]["token"], headers={"Sec-Fetch-Site": "cross-site"})[0], 403)
        invalid = [self.signal(**{"from": 2}), self.signal(names=["forged"]),
                   self.signal(type="answer"), self.signal(sdp="not SDP"),
                   self.signal(version=True), {"to": True, "signal": self.signal()["signal"]},
                   {"to": 0, "signal": "{}"}, {"to": 1, "signal": "not JSON"},
                   {"to": 1, "signal": "\ud800"}]
        for payload in invalid:
            with self.subTest(payload=payload):
                self.assertEqual(self.request(path, payload, players[0]["token"])[0], 400)
        self.expect_error(self.request(path, self.signal(sdp="v=0\r\n" + "a" * MAX_SIGNAL), players[0]["token"]), 413, "too_large")
        response, _ = self.raw("POST", path, "{}", {"Content-Type": "text/plain", "Authorization": f"Bearer {players[0]['token']}"})
        self.assertEqual(response.status, 415)
        self.assertEqual(self.request(path, self.signal(), players[0]["token"])[0], 200)
        self.expect_error(self.request(path, self.signal(), players[0]["token"]), 409, "duplicate_signal")
        self.expect_error(self.request(f"/api/rooms/{room}/nothing", {}, players[0]["token"]), 404, "not_found")

    @server(env={"P2SKAT_ROOM_TTL_MS": "1000"})
    def test_room_isolation_and_expiry(self):
        first, players = self.table()
        second, others = self.table()
        self.assertEqual(self.request(f"/api/rooms/{second}/signal", self.signal(), players[0]["token"])[0], 401)
        self.assertEqual(self.request(f"/api/rooms/{first}/leave", {}, others[0]["token"])[0], 401)
        self.stream(second, others[2])  # An attached stream keeps a table alive.
        time.sleep(1.5)
        self.assertEqual(self.request(f"/api/rooms/{first}/signal", self.signal(), players[0]["token"])[0], 404)
        self.assertEqual(self.request(f"/api/rooms/{second}/signal", self.signal(), others[0]["token"])[0], 200)

    @server(args=["--max-rooms", "1"])
    def test_global_cap_and_invalid_names(self):
        for body in ({"name": "", "browser": "ok"}, {"name": "x" * 41, "browser": "ok"},
                     {"name": "Ada", "browser": "x" * 49}, {"name": "Ada", "browser": "bad browser"},
                     {"name": "Ada", "browser": "ok", "seat": 2}, {"name": "Ada\u0007", "browser": "ok"},
                     {"name": "Ada", "browser": "ok", "mode": "tor"}, {"name": "Ada"}, [], "Ada"):
            with self.subTest(body=body):
                self.expect_error(self.request("/api/rooms", body), 400, "bad_request")
        response, _ = self.raw("POST", "/api/rooms", "{not json", {"Content-Type": "application/json"})
        self.assertEqual(response.status, 400)
        self.assertEqual(self.request("/api/rooms", {"name": "Ada", "browser": "ok"})[0], 200)
        self.expect_error(self.request("/api/rooms", {"name": "Bert", "browser": "ok"}), 503, "busy")
        self.assertEqual(self.request("/api/health", method="GET"), (200, {"ok": True}))

    @server(fast=True)
    def test_lost_stream_closes_relay_rooms_but_not_webrtc_rooms(self):
        webrtc, peers = self.table()
        relay, players = self.table("relay")
        webrtc_streams = [self.stream(webrtc, player) for player in peers]
        relay_streams = [self.stream(relay, player) for player in players]
        started = time.monotonic()
        webrtc_streams[1].close()
        relay_streams[1].close()
        with self.keepalive(relay, players):  # Bert's keepalives cannot replace his stream
            closed = relay_streams[0].closed_event(5)
        self.assertEqual(closed["message"], "Bert lost connection.")
        self.assertGreater(time.monotonic() - started, 1.2)
        self.assertEqual(relay_streams[2].closed_event(1), closed)
        # WebRTC games continue peer to peer when signaling drops.
        self.assertEqual(self.request(f"/api/rooms/{webrtc}/signal", self.signal(), peers[0]["token"])[0], 200)

    @server(fast=True)
    def test_heartbeat_is_a_named_ping_event(self):
        room, players = self.table()
        stream = self.stream(room, players[0])
        self.assertEqual(stream.data()["type"], "players")
        ping = stream.next(timeout=2, pings=True)
        self.assertEqual(ping, {"id": None, "event": "ping", "data": {"type": "ping"}})
        self.assertEqual(stream.retry, 2000)


class RelayTests(ConformanceTest):
    def test_mode_is_fixed_at_create(self):
        status, host = self.request("/api/rooms", {"name": "Ada", "browser": "b0", "mode": "relay"})
        self.assertEqual((status, host["mode"]), (200, "relay"))
        path = f"/api/rooms/{host['room']}/join"
        self.expect_error(self.request(path, {"name": "Bert", "browser": "b1", "mode": "webrtc"}), 400, "wrong_mode")
        self.expect_error(self.request(path, {"name": "Bert", "browser": "b1", "mode": "other"}), 400, "bad_request")
        status, bert = self.request(path, {"name": "Bert", "browser": "b1", "mode": "relay"})
        self.assertEqual((status, bert["seat"], bert["mode"]), (200, 1, "relay"))
        status, clara = self.request(path, {"name": "Clara", "browser": "b2"})
        self.assertEqual((status, clara["seat"], clara["mode"]), (200, 2, "relay"))
        status, other = self.request("/api/rooms", {"name": "Ada", "browser": "b0", "mode": "webrtc"})
        self.assertEqual((status, other["mode"]), (200, "webrtc"))

    def test_mode_mismatch_is_rejected(self):
        relay, players = self.table("relay")
        self.expect_error(self.request(f"/api/rooms/{relay}/signal", self.signal(), players[0]["token"]), 400, "wrong_mode")
        webrtc, peers = self.table()
        self.expect_error(self.relay(webrtc, peers[0], [{"to": 1, "seq": 1, "payload": "AAAA"}]), 400, "wrong_mode")

    def test_relay_forwards_with_server_stamped_sender(self):
        room, players = self.table("relay")
        stream = self.stream(room, players[1])
        self.assertEqual(stream.data()["type"], "players")
        self.assertEqual(self.relay(room, players[0], [{"to": 1, "seq": 1, "payload": "aGVsbG8"}]),
                         (200, {"expected": {"1": 2, "2": 1}}))
        self.assertEqual(stream.next(), {"id": 1, "event": "message",
                                         "data": {"type": "relay", "from": 0, "seq": 1, "payload": "aGVsbG8"}})
        self.assertEqual(self.relay(room, players[2], [{"to": 1, "seq": 1, "payload": "AAAA"}])[0], 200)
        self.assertEqual(stream.data(), {"type": "relay", "from": 2, "seq": 1, "payload": "AAAA"})
        invalid = [{"to": 1, "seq": 2, "payload": "AAAA", "from": 2}, {"to": 0, "seq": 1, "payload": "AAAA"},
                   {"to": 3, "seq": 1, "payload": "AAAA"}, {"to": True, "seq": 1, "payload": "AAAA"},
                   {"to": 1, "seq": 0, "payload": "AAAA"}, {"to": 1, "seq": True, "payload": "AAAA"},
                   {"to": 1, "seq": 1.0, "payload": "AAAA"}, {"to": 1, "seq": 2 ** 53, "payload": "AAAA"},
                   {"to": 1, "seq": 2, "payload": "AAA="}, {"to": 1, "seq": 2, "payload": "A"},
                   {"to": 1, "seq": 2, "payload": ""}, {"to": 1, "seq": 2, "payload": "a+b/"},
                   {"to": 1, "seq": 2, "payload": 5}, {"to": 1, "seq": 2}, "frame"]
        for frame in invalid:
            with self.subTest(frame=frame):
                self.expect_error(self.relay(room, players[0], [frame]), 400, "bad_request")
        path = f"/api/rooms/{room}/relay"
        for body in ({}, {"frames": {}}, {"frames": [], "extra": 1}, {"frames": [], "ack": -1},
                     {"frames": [], "ack": True}, {"frames": [], "ack": 99}):
            with self.subTest(body=body):
                self.expect_error(self.request(path, body, players[0]["token"]), 400, "bad_request")
        self.expect_error(self.request(path, {"frames": []}, "forged"), 401, "unauthorized")
        self.assertIsNone(stream.next(0.5), "Rejected frames must not be forwarded")

    def test_duplicate_frames_are_skipped(self):
        room, players = self.table("relay")
        frames = [{"to": 1, "seq": 1, "payload": "AQ"}, {"to": 1, "seq": 2, "payload": "Ag"}]
        self.assertEqual(self.relay(room, players[0], frames), (200, {"expected": {"1": 3, "2": 1}}))
        self.assertEqual(self.relay(room, players[0], frames), (200, {"expected": {"1": 3, "2": 1}}))
        self.assertEqual(self.relay(room, players[0], frames[1:] + [{"to": 1, "seq": 3, "payload": "Aw"}]),
                         (200, {"expected": {"1": 4, "2": 1}}))
        stream = self.stream(room, players[1])
        stream.data()
        received = [stream.next() for _ in range(3)]
        self.assertEqual([(event["id"], event["data"]["seq"], event["data"]["payload"]) for event in received],
                         [(1, 1, "AQ"), (2, 2, "Ag"), (3, 3, "Aw")])
        self.assertIsNone(stream.next(0.5))

    def test_gap_rejects_the_whole_batch(self):
        room, players = self.table("relay")
        result = self.relay(room, players[0], [{"to": 1, "seq": 1, "payload": "AQ"}, {"to": 2, "seq": 2, "payload": "Ag"}])
        self.expect_error(result, 422, "gap")
        self.assertEqual(result[1]["expected"], {"1": 1, "2": 1})
        self.assertEqual(self.relay(room, players[0], [{"to": 1, "seq": 1, "payload": "Aw"}]), (200, {"expected": {"1": 2, "2": 1}}))
        bert, clara = self.stream(room, players[1]), self.stream(room, players[2])
        bert.data()
        clara.data()
        self.assertEqual(bert.next(), {"id": 1, "event": "message", "data": {"type": "relay", "from": 0, "seq": 1, "payload": "Aw"}})
        self.assertIsNone(clara.next(0.5))
        self.assertIsNone(bert.next(0.1))

    def test_batch_is_delivered_in_order_per_destination(self):
        room, players = self.table("relay")
        bert, clara = self.stream(room, players[1]), self.stream(room, players[2])
        bert.data()
        clara.data()
        frames = [{"to": 1, "seq": 1, "payload": "AQ"}, {"to": 2, "seq": 1, "payload": "Ag"},
                  {"to": 1, "seq": 2, "payload": "Aw"}, {"to": 2, "seq": 2, "payload": "BA"},
                  {"to": 1, "seq": 3, "payload": "BQ"}]
        self.assertEqual(self.relay(room, players[0], frames), (200, {"expected": {"1": 4, "2": 3}}))
        self.assertEqual([(event["id"], event["data"]["payload"]) for event in (bert.next() for _ in range(3))],
                         [(1, "AQ"), (2, "Aw"), (3, "BQ")])
        self.assertEqual([(event["id"], event["data"]["payload"]) for event in (clara.next() for _ in range(2))],
                         [(1, "Ag"), (2, "BA")])
        # Replies flow the other way on the same table.
        self.assertEqual(self.relay(room, players[1], [{"to": 0, "seq": 1, "payload": "Bg"}]), (200, {"expected": {"0": 2, "2": 1}}))

    def test_payload_cap(self):
        room, players = self.table("relay")
        largest = payload(MAX_PAYLOAD)
        self.assertEqual(self.relay(room, players[0], [{"to": 1, "seq": 1, "payload": largest}])[0], 200)
        self.expect_error(self.relay(room, players[0], [{"to": 1, "seq": 2, "payload": payload(MAX_PAYLOAD + 1)}]), 413, "too_large")
        stream = self.stream(room, players[1])
        stream.data()
        self.assertEqual(stream.data()["payload"], largest)
        self.assertIsNone(stream.next(0.3))

    def test_resume_with_after_and_last_event_id(self):
        room, players = self.table("relay")
        frames = [{"to": 1, "seq": seq, "payload": "AAAA"} for seq in (1, 2, 3)]
        self.assertEqual(self.relay(room, players[0], frames)[0], 200)
        first = self.stream(room, players[1], after=1)
        self.assertEqual(first.data()["type"], "players")
        self.assertEqual([first.next()["id"], first.next()["id"]], [2, 3])
        second = self.stream(room, players[1], last_event_id=2)
        self.assertTrue(first.ended(), "A new stream must replace the old one")
        second.data()
        self.assertEqual(second.next()["data"]["seq"], 3)
        # EventSource reconnects with its original URL plus a newer Last-Event-ID; the larger id wins.
        third = self.stream(room, players[1], after=1, last_event_id=3)
        third.data()
        self.assertIsNone(third.next(0.3))
        self.expect_error((400, self.stream(room, players[1], after=9, status=400)), 400, "bad_request")
        self.expect_error((400, self.stream(room, players[1], after="x", status=400)), 400, "bad_request")
        # Acknowledged events are gone: a fresh stream starts after them.
        fourth = self.stream(room, players[1], after=0)
        fourth.data()
        self.assertEqual(self.relay(room, players[0], [{"to": 1, "seq": 4, "payload": "AAAA"}])[0], 200)
        self.assertEqual(fourth.next()["id"], 4)
        self.expect_error((401, self.stream(room, {"token": "forged"}, status=401)), 401, "unauthorized")

    def test_ack_in_relay_post_trims_the_log(self):
        room, players = self.table("relay")
        self.assertEqual(self.relay(room, players[0], [{"to": 1, "seq": seq, "payload": "AAAA"} for seq in (1, 2, 3)])[0], 200)
        self.expect_error(self.relay(room, players[1], [], ack=4), 400, "bad_request")
        self.assertEqual(self.relay(room, players[1], [], ack=2), (200, {"expected": {"0": 1, "2": 1}}))
        stream = self.stream(room, players[1])
        stream.data()
        self.assertEqual(stream.next()["id"], 3)
        self.assertIsNone(stream.next(0.3))

    def test_closed_room_replays_closed_event(self):
        room, players = self.table("relay")
        self.assertEqual(self.request(f"/api/rooms/{room}/leave", {}, players[2]["token"]), (200, {"ok": True}))
        self.expect_error(self.relay(room, players[0], []), 404, "room_closed")
        stream = self.stream(room, players[0])
        self.assertEqual(stream.data(), {"type": "closed", "message": "Clara left the table."})
        self.assertTrue(stream.ended())
        self.expect_error(self.request("/api/rooms/unknown-room-id-123456/relay", {"frames": []}, players[0]["token"]), 404, "not_found")

    @server(env={"P2SKAT_LOG_EVENTS": "4"})
    def test_delivered_events_are_trimmed_and_missed_events_close_the_room(self):
        room, players = self.table("relay")
        stream = self.stream(room, players[1])
        stream.data()
        for seq in range(1, 11):
            self.assertEqual(self.relay(room, players[0], [{"to": 1, "seq": seq, "payload": "AAAA"}])[0], 200)
            self.assertEqual(stream.next()["id"], seq)
            time.sleep(0.02)
        # Events 1..6 were discarded unacknowledged; resuming before them cannot work.
        resumed = self.stream(room, players[1], after=2)
        self.assertEqual(resumed.data(), {"type": "closed", "message": "Bert missed messages after a connection problem."})

    @server(env={"P2SKAT_LOG_EVENTS": "8", "P2SKAT_RATE": "1000", "P2SKAT_BURST": "1000"})
    def test_undelivered_event_limit_closes_the_room(self):
        room, players = self.table("relay")
        bert = self.stream(room, players[1])
        bert.data()
        frames = [{"to": 2, "seq": seq, "payload": "AAAA"} for seq in range(1, 10)]
        result = self.relay(room, players[0], frames)
        self.expect_error(result, 429, "over_limit")
        self.assertIn("Clara", result[1]["error"])
        self.assertEqual(bert.data(), {"type": "closed", "message": result[1]["error"]})
        self.expect_error(self.relay(room, players[0], []), 404, "room_closed")

    @server(env={"P2SKAT_LOG_BYTES": "4096"})
    def test_log_byte_limit_closes_the_room(self):
        room, players = self.table("relay")
        for seq in (1, 2):
            self.assertEqual(self.relay(room, players[0], [{"to": 2, "seq": seq, "payload": payload(1000)}])[0], 200)
        self.expect_error(self.relay(room, players[0], [{"to": 2, "seq": 3, "payload": payload(1000)}]), 429, "over_limit")

    @server(env={"P2SKAT_RATE": "5", "P2SKAT_BURST": "5"})
    def test_rate_limit_closes_the_room(self):
        room, players = self.table("relay")
        self.assertEqual(self.relay(room, players[0], [{"to": 1, "seq": seq, "payload": "AAAA"} for seq in range(1, 5)])[0], 200)
        result = self.relay(room, players[0], [{"to": 1, "seq": seq, "payload": "AAAA"} for seq in range(5, 8)])
        self.expect_error(result, 429, "over_limit")
        self.assertIn("Ada sent messages too quickly", result[1]["error"])
        stream = self.stream(room, players[1])
        self.assertEqual(stream.data()["type"], "closed")


    @server(fast=True)
    def test_open_stream_without_keepalive_posts_is_lost(self):
        # A frozen (e.g. SIGSTOPped) tab keeps its stream open but stops posting.
        room, players = self.table("relay")
        streams = [self.stream(room, player) for player in players]
        started = time.monotonic()
        with self.keepalive(room, [players[0], players[2]]):
            closed = streams[0].closed_event(5)
        self.assertEqual(closed["message"], "Bert lost connection.")
        self.assertGreater(time.monotonic() - started, 1.2)
        self.assertEqual(streams[1].closed_event(1), closed)

    @server(fast=True)
    def test_empty_keepalive_posts_keep_a_table_alive(self):
        room, players = self.table("relay")
        streams = [self.stream(room, player) for player in players]
        for stream in streams:
            self.assertEqual(stream.data()["type"], "players")
        with self.keepalive(room, players) as statuses:
            time.sleep(3.5)  # more than twice the 1.5 s liveness window
        self.assertTrue(statuses and all(status == 200 for status in statuses), statuses)
        self.assertIsNone(streams[0].next(0.2), "The table must stay open")
        self.assertEqual(self.relay(room, players[0], [{"to": 1, "seq": 1, "payload": "AAAA"}])[0], 200)

    @server(env={"P2SKAT_BODY_TIMEOUT_MS": "1000"})
    def test_stalled_body_is_cut_off(self):
        room, players = self.table("relay")
        for sent in (b"", b'{"frames"'):
            with self.subTest(sent=sent):
                sock = self.raw_post_headers(f"/api/rooms/{room}/relay", 100, players[0]["token"])
                sock.sendall(sent)
                started = time.monotonic()
                sock.settimeout(4)
                try:
                    data = sock.recv(4096)
                except ConnectionResetError:
                    data = b""
                sock.close()
                self.assertEqual(data, b"", "A stalled body gets no response; the connection closes")
                self.assertLess(time.monotonic() - started, 3.5)
                self.assertGreater(time.monotonic() - started, 0.7)

    def test_seat_is_checked_before_the_body_is_read(self):
        room, players = self.table("relay")
        # No body follows, so an answer proves the server did not wait for it.
        for path, token, status in ((f"/api/rooms/{room}/relay", "forged", 401),
                                    ("/api/rooms/unknown-room-id-123456/relay", players[0]["token"], 404),
                                    (f"/api/rooms/{room}/relay", None, 401)):
            with self.subTest(path=path, token=token):
                sock = self.raw_post_headers(path, 1024 * 1024, token)
                sock.settimeout(2)
                response = http.client.HTTPResponse(sock)
                response.begin()
                self.assertEqual(response.status, status)
                response.close()
                sock.close()
        # Create and join take no token, so their bodies are capped at 4 KiB.
        body = {"name": "Ada", "browser": "b0", "padding": "x" * 5000}
        self.expect_error(self.request("/api/rooms", body), 413, "too_large")
        self.expect_error(self.request(f"/api/rooms/{room}/join", body), 413, "too_large")

    @server(env={"P2SKAT_TOTAL_BYTES": "6000"}, args=["--max-rooms", "2"])
    def test_one_table_cannot_use_up_another_tables_share(self):
        first, players = self.table("relay")
        second, others = self.table("relay")
        for seq in (1, 2):
            self.assertEqual(self.relay(first, players[0], [{"to": 1, "seq": seq, "payload": payload(1000)}])[0], 200)
        result = self.relay(first, players[0], [{"to": 1, "seq": 3, "payload": payload(1000)}])
        self.expect_error(result, 429, "over_limit")
        self.assertIn("This table ran out of message space", result[1]["error"])
        for seq in (1, 2):
            self.assertEqual(self.relay(second, others[0], [{"to": 1, "seq": seq, "payload": payload(1000)}])[0], 200)


class RelayOnlyTests(ConformanceTest):
    def check_relay_only(self):
        for body in ({"name": "Ada", "browser": "b0"}, {"name": "Ada", "browser": "b0", "mode": "webrtc"}):
            with self.subTest(body=body):
                self.expect_error(self.request("/api/rooms", body), 400, "relay_only")
        status, host = self.request("/api/rooms", {"name": "Ada", "browser": "b0", "mode": "relay"})
        self.assertEqual((status, host["mode"]), (200, "relay"))

    @server(args=["--relay-only"])
    def test_relay_only_flag_refuses_webrtc_tables(self):
        self.check_relay_only()

    @server(env={"P2SKAT_RELAY_ONLY": "1"})
    def test_relay_only_environment_refuses_webrtc_tables(self):
        self.check_relay_only()


class StaticTests(ConformanceTest):
    def test_static_files_and_security_headers(self):
        response, body = self.raw("GET", "/")
        self.assertEqual(response.status, 200)
        self.assertTrue(response.getheader("Content-Type").startswith("text/html"))
        self.assertIn(b"<script type=\"module\" src=\"app.js\">", body)
        self.assertEqual(response.getheader("Content-Security-Policy"), CSP)
        self.assertEqual(response.getheader("X-Content-Type-Options"), "nosniff")
        self.assertEqual(response.getheader("Referrer-Policy"), "no-referrer")
        for path in ("/app.js", "/rooms.mjs", "/worker.js"):
            response, _ = self.raw("GET", path)
            self.assertEqual(response.status, 200, path)
            self.assertTrue(response.getheader("Content-Type").startswith("text/javascript"), path)
        response, _ = self.raw("GET", "/style.css")
        self.assertTrue(response.getheader("Content-Type").startswith("text/css"))
        if (ROOT / "web" / "pkg" / "p2skat_bg.wasm").exists():
            response, _ = self.raw("GET", "/pkg/p2skat_bg.wasm")
            self.assertEqual(response.getheader("Content-Type"), "application/wasm")
        response, _ = self.raw("GET", "/api/health")
        self.assertEqual(response.getheader("Cache-Control"), "no-store")
        self.assertEqual(response.getheader("X-Content-Type-Options"), "nosniff")
        response, _ = self.raw("HEAD", "/index.html")
        self.assertEqual(response.status, 200)

    def test_path_traversal_and_missing_files(self):
        for path in ("/../scripts/serve.py", "/%2e%2e/scripts/serve.py", "/pkg/..%2f..%2fscripts%2fserve.py",
                     "/..%5cscripts%5cserve.py", "/missing.js", "/pkg/", "/api/unknown"):
            with self.subTest(path=path):
                response, body = self.raw("GET", path)
                self.assertIn(response.status, (400, 403, 404))
                self.assertNotIn(b"def main", body)

    @server(args=["--allowed-host", "table.example.onion"])
    def test_allowed_host(self):
        for host in ("table.example.onion", "table.example.onion:80", "TABLE.example.onion"):
            with self.subTest(host=host):
                response, _ = self.raw("GET", "/api/health", headers={"Host": host})
                self.assertEqual(response.status, 200)
        for path in ("/api/health", "/"):
            response, body = self.raw("GET", path)
            self.assertEqual(response.status, 421)
            self.assertEqual(json.loads(body)["code"], "wrong_host")
        status, _ = self.request("/api/rooms", {"name": "Ada", "browser": "b0"},
                                 origin="http://table.example.onion", headers={"Host": "table.example.onion"})
        self.assertEqual(status, 200)


class FaultInjectionTests(ConformanceTest):
    @server(env={"P2SKAT_FAULT_LATENCY_MS": "300,100"})
    def test_latency_delays_posts_and_events(self):
        started = time.monotonic()
        room, players = self.table("relay")
        self.assertGreater(time.monotonic() - started, 3 * 0.19)
        stream = self.stream(room, players[1])
        stream.data()
        started = time.monotonic()
        self.assertEqual(self.relay(room, players[0], [{"to": 1, "seq": 1, "payload": "AAAA"}])[0], 200)
        self.assertEqual(stream.data()["seq"], 1)
        self.assertGreater(time.monotonic() - started, 0.19)

    @server(env={"P2SKAT_FAULT_DROP_S": "0.4"})
    def test_drop_kills_streams_and_resume_works(self):
        room, players = self.table("relay")
        self.assertEqual(self.relay(room, players[0], [{"to": 1, "seq": 1, "payload": "AAAA"}])[0], 200)
        stream = self.stream(room, players[1])
        stream.data()
        self.assertEqual(stream.next()["id"], 1)
        self.assertTrue(stream.ended(3), "The drop fault must cut the stream")
        resumed = self.stream(room, players[1], after=1)
        self.assertEqual(resumed.data()["type"], "players")


if __name__ == "__main__":
    if sys.argv[1:] == ["--print-env"]:
        print(" ".join(f"{key}={value}" for key, value in FAST_ENV.items()))
        sys.exit(0)
    unittest.main()
