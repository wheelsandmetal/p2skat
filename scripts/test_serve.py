"""Signaling boundary tests using real HTTP requests and SSE streams."""
import concurrent.futures
import http.client
import json
import threading
import time
import unittest

from serve import Handler, MAX_SIGNAL, Player, Room, Server


class SignalingTests(unittest.TestCase):
    def setUp(self):
        self.server = Server(("127.0.0.1", 0))
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.origin = f"http://127.0.0.1:{self.server.server_port}"
        self.streams = []

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        for connection, response in self.streams:
            response.close()
            connection.close()
        self.thread.join()

    def request(self, path, body=None, token=None, origin=None, method="POST"):
        connection = http.client.HTTPConnection("127.0.0.1", self.server.server_port, timeout=3)
        headers = {"Content-Type": "application/json", "Origin": origin or self.origin}
        if token:
            headers["Authorization"] = f"Bearer {token}"
        connection.request(method, path, None if body is None else json.dumps(body), headers)
        response = connection.getresponse()
        status, payload = response.status, json.loads(response.read())
        connection.close()
        return status, payload

    def table(self):
        status, host = self.request("/api/rooms", {"name": "Ada", "browser": "browser0"})
        self.assertEqual(status, 200)
        players = [host]
        for index, name in enumerate(("Bert", "Clara"), 1):
            status, player = self.request(f"/api/rooms/{host['room']}/join", {"name": name, "browser": f"browser{index}"})
            self.assertEqual(status, 200)
            players.append(player)
        return host["room"], players

    def stream(self, room, player):
        connection = http.client.HTTPConnection("127.0.0.1", self.server.server_port, timeout=3)
        connection.request("GET", f"/api/rooms/{room}/events?token={player['token']}")
        response = connection.getresponse()
        self.assertEqual(response.status, 200)
        self.assertEqual(response.getheader("Content-Type"), "text/event-stream")
        self.streams.append((connection, response))
        return response

    def event(self, stream):
        while True:
            line = stream.readline()
            self.assertTrue(line, "SSE closed without an event")
            if line.startswith(b"data: "):
                return json.loads(line[6:])

    def signal(self, source=0, destination=1, **changes):
        signal = {"version": 1, "from": source, "to": destination,
                  "names": ["Ada", "Bert", "Clara"],
                  "type": "offer" if source < destination else "answer", "sdp": "v=0\r\ns=test\r\n"}
        signal.update(changes)
        return {"to": destination, "signal": json.dumps(signal)}

    def test_join_assigns_atomic_fixed_seats_and_capacity(self):
        status, host = self.request("/api/rooms", {"name": "Ada", "browser": "same-browser"})
        self.assertEqual(status, 200)
        path = f"/api/rooms/{host['room']}/join"
        with concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:
            results = list(pool.map(lambda index: self.request(path, {"name": f"Player{index}", "browser": "same-browser"}), range(3)))
        successful = [player for status, player in results if status == 200]
        self.assertEqual(sorted(player["seat"] for player in successful), [1, 2])
        self.assertEqual(sum(status == 409 for status, _ in results), 1)
        self.assertEqual(len({host["token"], *(player["token"] for player in successful)}), 3)
        self.assertTrue(all("token" not in player for player in successful[-1]["players"]))

    def test_sse_roster_forwarding_and_leave(self):
        room, players = self.table()
        stream = self.stream(room, players[1])
        self.assertEqual(self.event(stream), {"type": "players", "players": players[2]["players"]})
        payload = self.signal()
        self.assertEqual(self.request(f"/api/rooms/{room}/signal", payload, players[0]["token"])[0], 200)
        self.assertEqual(self.event(stream), {"type": "signal", "from": 0, "signal": payload["signal"]})
        self.assertEqual(self.request(f"/api/rooms/{room}/leave", {}, players[0]["token"])[0], 200)
        self.assertEqual(self.event(stream)["type"], "closed")
        self.assertEqual(self.request(f"/api/rooms/{room}/signal", self.signal(1, 2), players[1]["token"])[0], 404)

    def test_sse_roster_updates_and_duplicate_subscriber(self):
        _, host = self.request("/api/rooms", {"name": "Ada", "browser": "browser0"})
        room = host["room"]
        stream = self.stream(room, host)
        self.assertEqual(len(self.event(stream)["players"]), 1)
        self.assertEqual(self.request(f"/api/rooms/{room}/events?token={host['token']}", method="GET")[0], 409)
        for index, name in enumerate(("Bert", "Clara"), 1):
            self.assertEqual(self.request(f"/api/rooms/{room}/join", {"name": name, "browser": f"browser{index}"})[0], 200)
            self.assertEqual(len(self.event(stream)["players"]), index + 1)

    def test_sse_header_disconnect_cleans_subscriber_without_ending_game(self):
        room = Room()
        player = Player(0, "Ada", "browser0")
        room.players.append(player)
        player.streaming = True

        class Disconnected:
            server = self.server
            def send_response(self, _status): pass
            def send_header(self, _name, _value): pass
            def end_headers(self): raise BrokenPipeError()

        Handler.events(Disconnected(), room, player, {"type": "players", "players": room.roster()})
        self.assertFalse(player.streaming)
        self.assertFalse(room.closed, "Signaling loss must not end a healthy peer game")

    def test_signal_retained_before_sse_connects(self):
        room, players = self.table()
        payload = self.signal()
        self.assertEqual(self.request(f"/api/rooms/{room}/signal", payload, players[0]["token"])[0], 200)
        stream = self.stream(room, players[1])
        self.assertEqual(len(self.event(stream)["players"]), 3)
        self.assertEqual(self.event(stream)["signal"], payload["signal"])

    def test_sender_payload_and_origin_validation(self):
        room, players = self.table()
        path = f"/api/rooms/{room}/signal"
        self.assertEqual(self.request(path, self.signal(), "forged-token")[0], 401)
        self.assertEqual(self.request(path, self.signal(), "\u00ff")[0], 401)
        self.assertEqual(self.request(path, self.signal(), players[0]["token"], origin="https://other.example")[0], 403)
        invalid = [self.signal(**{"from": 2}), self.signal(names=["forged"]),
                   self.signal(type="answer"), self.signal(sdp="not SDP"),
                   self.signal(version=True), {"to": True, "signal": self.signal()["signal"]},
                   {"to": 0, "signal": "{}"}, {"to": 1, "signal": "not JSON"},
                   {"to": 1, "signal": "\ud800"}]
        for payload in invalid:
            with self.subTest(payload=payload):
                self.assertEqual(self.request(path, payload, players[0]["token"])[0], 400)
        self.assertEqual(self.request(path, self.signal(sdp="v=0\r\n" + "a" * MAX_SIGNAL), players[0]["token"])[0], 413)
        self.assertEqual(self.request(path, self.signal(), players[0]["token"])[0], 200)
        self.assertEqual(self.request(path, self.signal(), players[0]["token"])[0], 409)

    def test_room_isolation_and_expiry(self):
        first, players = self.table()
        second, others = self.table()
        self.assertEqual(self.request(f"/api/rooms/{second}/signal", self.signal(), players[0]["token"])[0], 401)
        self.assertEqual(self.request(f"/api/rooms/{first}/leave", {}, others[0]["token"])[0], 401)
        with self.server.lock:
            self.server.rooms[first].updated = time.monotonic() - self.server.ttl - 1
        self.assertEqual(self.request(f"/api/rooms/{first}/signal", self.signal(), players[0]["token"])[0], 404)
        self.assertEqual(self.request(f"/api/rooms/{second}/signal", self.signal(), others[0]["token"])[0], 200)

    def test_global_cap_and_invalid_names(self):
        for body in ({"name": "", "browser": "ok"}, {"name": "x" * 41, "browser": "ok"},
                     {"name": "Ada", "browser": "x" * 49}, {"name": "Ada", "browser": "bad browser"},
                     {"name": "Ada", "browser": "ok", "seat": 2}):
            self.assertEqual(self.request("/api/rooms", body)[0], 400)
        self.server.max_rooms = 1
        self.assertEqual(self.request("/api/rooms", {"name": "Ada", "browser": "ok"})[0], 200)
        self.assertEqual(self.request("/api/rooms", {"name": "Bert", "browser": "ok"})[0], 503)
        self.assertEqual(self.request("/api/health", method="GET"), (200, {"ok": True}))


if __name__ == "__main__":
    unittest.main()
