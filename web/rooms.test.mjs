import test from 'node:test';
import assert from 'node:assert/strict';
import { RoomClient, browserMarker, parseRoomEvent, validatePlayers } from './rooms.mjs';
import { validateNames } from './transport.mjs';

const roomId = 'unguessable-room-id';
const token = 'unguessable-seat-token';
const players = [{ seat: 0, name: 'Ada', browser: 'browser-identifier-1' }];
const settleMicrotasks = () => new Promise(resolve => setTimeout(resolve, 0));
const response = (data, status = 200, type = 'application/json') => ({
  ok: status < 400, status, headers: { get: () => type }, json: async () => data,
});

test('browser tabs reuse a random browser marker and disabled storage has a secure fallback', () => {
  const values = new Map();
  const storage = { getItem: key => values.get(key), setItem: (key, value) => values.set(key, value) };
  let generated = 0;
  const crypto = { randomUUID: () => `secure-browser-id-${++generated}` };
  assert.equal(browserMarker(storage, crypto), 'secure-browser-id-1');
  assert.equal(browserMarker(storage, crypto), 'secure-browser-id-1');
  const blocked = { getItem: () => { throw new Error('disabled'); }, setItem: () => { throw new Error('disabled'); } };
  assert.equal(browserMarker(blocked, crypto), 'secure-browser-id-2');
});

test('room roster rejects reused seats and sorts authoritative seats', () => {
  assert.throws(() => validatePlayers([...players, ...players]), /invalid player/);
  const third = { seat: 2, name: '<script>', browser: 'browser-3' };
  assert.deepEqual(validatePlayers([third, ...players]), [...players, third]);
  assert.throws(() => validatePlayers([{ ...players[0], seat: 3 }]), /invalid player/);
  assert.throws(() => validatePlayers([{ ...players[0], name: ' ' }]), /invalid player/);
});

test('room events validate sender identity and bound signaling before reaching the mesh', () => {
  assert.deepEqual(parseRoomEvent(JSON.stringify({ type: 'players', players }), 0).players, players);
  assert.throws(() => parseRoomEvent(JSON.stringify({ type: 'signal', from: 0, signal: '{}' }), 0), /invalid event/);
  assert.throws(() => parseRoomEvent(JSON.stringify({ type: 'signal', from: 1, signal: 'x'.repeat(65537) }), 0), /too large/);
  assert.throws(() => parseRoomEvent('{"type":"unexpected"}', 0), /invalid event/);
});

test('room client authenticates signaling and leave and closes its event stream', async () => {
  const requests = [];
  const fetcher = async (path, options) => {
    requests.push({ path, options });
    return response(path === '/api/rooms' ? { room: roomId, token, seat: 0, players } : { ok: true });
  };
  class EventSource {
    constructor(url) { this.url = url; }
    close() { this.closed = true; }
  }
  const client = await RoomClient.enter({ name: 'Ada', browser: players[0].browser, fetcher, EventSourceClass: EventSource });
  const events = [];
  const errors = [];
  client.listen(event => events.push(event), error => errors.push(error));
  assert.ok(client.source.url.includes(`token=${token}`));
  client.source.onmessage({ data: JSON.stringify({ type: 'players', players }) });
  assert.equal(events.length, 1);
  await client.sendSignal(1, '{"offer":true}');
  assert.equal(requests[1].options.headers.Authorization, `Bearer ${token}`);
  assert.deepEqual(JSON.parse(requests[1].options.body), { to: 1, signal: '{"offer":true}' });
  await client.close();
  assert.equal(client.source.closed, true);
  assert.equal(requests[2].options.keepalive, true);
  client.source.onmessage?.({ data: '{"type":"bad"}' });
  assert.deepEqual(errors, []);
  await assert.rejects(() => client.sendSignal(1, '{}'), /ended/);
});

test('room stream failure is reported once and disables automatic replay', async () => {
  class EventSource { close() { this.closed = true; } }
  const client = new RoomClient({ room: roomId, token, seat: 0 }, async () => response({}), EventSource);
  const errors = [];
  client.listen(() => {}, error => errors.push(error.message));
  client.source.onerror();
  assert.equal(client.source.closed, true);
  assert.equal(client.source.onerror, null);
  assert.match(errors[0], /lost contact/);
});

test('an old static server gives concrete restart instructions instead of a JSON parse error', async () => {
  await assert.rejects(() => RoomClient.enter({ name: 'Ada', browser: players[0].browser,
    fetcher: async () => response(null, 404, 'text/html') }), /python3 scripts\/serve.py/);
  await assert.rejects(() => RoomClient.enter({ room: '../bad', name: 'Ada', browser: players[0].browser }), /invite link is invalid/);
});

test('seat response must belong to this player and browser', async () => {
  await assert.rejects(() => RoomClient.enter({ name: 'Someone else', browser: players[0].browser,
    fetcher: async () => response({ room: roomId, token, seat: 0, players }) }), /another player/);
});

// Relay tables -----------------------------------------------------------------

class FakeTimers {
  constructor() { this.tasks = new Map(); this.next = 1; }
  setTimeout(fn, ms) { const id = this.next++; this.tasks.set(id, { fn, ms, repeat: false }); return id; }
  setInterval(fn, ms) { const id = this.next++; this.tasks.set(id, { fn, ms, repeat: true }); return id; }
  clearTimeout(id) { this.tasks.delete(id); }
  clearInterval(id) { this.tasks.delete(id); }
  run() { for (const [id, task] of [...this.tasks]) { if (!task.repeat) this.tasks.delete(id); task.fn(); } }
}

class FakeEventSource {
  static instances = [];
  constructor(url) { this.url = url; this.readyState = 0; this.listeners = {}; FakeEventSource.instances.push(this); }
  addEventListener(type, fn) { this.listeners[type] = fn; }
  close() { this.closed = true; this.readyState = 2; }
}

test('relay seats ask for relay mode and refuse a server that answers with another mode', async () => {
  const bodies = [];
  const fetcher = mode => async (path, options) => {
    bodies.push(JSON.parse(options.body));
    return response({ room: roomId, token, seat: 0, players, mode });
  };
  const client = await RoomClient.enter({ name: 'Ada', browser: players[0].browser, mode: 'relay', fetcher: fetcher('relay') });
  assert.equal(client.mode, 'relay');
  assert.equal(bodies[0].mode, 'relay');
  await assert.rejects(() => RoomClient.enter({ name: 'Ada', browser: players[0].browser, mode: 'relay', fetcher: fetcher('webrtc') }),
    /Refusing to connect directly/);
  await assert.rejects(() => RoomClient.enter({ name: 'Ada', browser: players[0].browser, mode: 'relay', fetcher: fetcher(undefined) }),
    /Refusing to connect directly/);
  await assert.rejects(() => RoomClient.enter({ room: 'another-room-identifier', name: 'Ada', browser: players[0].browser, mode: 'relay', fetcher: fetcher('relay') }),
    /invalid seat details/, 'the server must seat us at the invited table');
  const wrong = await RoomClient.enter({ name: 'Ada', browser: players[0].browser, mode: 'relay',
    fetcher: async () => response({ error: 'This table uses webrtc mode', code: 'wrong_mode' }, 400) }).catch(error => error);
  assert.equal(wrong.code, 'wrong_mode');
});

test('relay events are validated, and each kind of table rejects the other kind of event', () => {
  const relay = { type: 'relay', from: 1, seq: 2, payload: 'abc-_' };
  assert.deepEqual(parseRoomEvent(JSON.stringify(relay), 0, 'relay'), relay);
  assert.deepEqual(parseRoomEvent('{"type":"ping"}', 0, 'relay'), { type: 'ping' });
  assert.throws(() => parseRoomEvent(JSON.stringify(relay), 0, 'webrtc'), /invalid event/);
  assert.throws(() => parseRoomEvent(JSON.stringify({ type: 'signal', from: 1, signal: '{}' }), 0, 'relay'), /invalid event/);
  for (const bad of [{ from: 0 }, { seq: 0 }, { seq: 1.5 }, { payload: 'a=' }, { payload: '' }, { payload: 'a'.repeat(87468) }]) {
    assert.throws(() => parseRoomEvent(JSON.stringify({ ...relay, ...bad }), 0, 'relay'), /invalid event|too large/);
  }
});

test('the relay event stream drops replays, resumes from the last id, and survives a dead stream', () => {
  FakeEventSource.instances = [];
  const timers = new FakeTimers();
  const client = new RoomClient({ room: roomId, token, seat: 0, mode: 'relay' }, async () => response({}), FakeEventSource, timers);
  let clock = 0;
  client.now = () => clock;
  const events = [];
  const errors = [];
  client.listen(event => events.push(event), error => errors.push(error.message));
  const first = FakeEventSource.instances[0];
  assert.doesNotMatch(first.url, /after=/);
  const relay = seq => JSON.stringify({ type: 'relay', from: 1, seq, payload: 'AAAA' });
  first.onmessage({ data: JSON.stringify({ type: 'players', players }), lastEventId: '' });
  first.onmessage({ data: relay(1), lastEventId: '1' });
  first.onmessage({ data: relay(2), lastEventId: '2' });
  first.onmessage({ data: JSON.stringify({ type: 'players', players }), lastEventId: '2' }); // the id buffer persists
  first.onmessage({ data: relay(2), lastEventId: '2' });
  assert.deepEqual(events.map(event => event.type), ['players', 'relay', 'relay', 'players']);
  assert.equal(client.lastEventId, 2);

  clock = 20000;
  first.listeners.ping();
  timers.run();
  assert.equal(FakeEventSource.instances.length, 1, 'pings keep the stream alive');
  clock = 46000;
  timers.run();
  assert.equal(FakeEventSource.instances.length, 2, 'the watchdog reopens a silent stream');
  assert.ok(first.closed);
  const second = FakeEventSource.instances[1];
  assert.match(second.url, /after=2$/);
  second.onmessage({ data: relay(3), lastEventId: '3' });
  assert.equal(events.length, 5);

  second.readyState = 0;
  second.onerror();
  assert.equal(FakeEventSource.instances.length, 2, 'EventSource retries by itself while CONNECTING');
  second.readyState = 2;
  second.onerror();
  timers.run();
  assert.equal(FakeEventSource.instances.length, 3, 'a stream EventSource gave up on is reopened by hand');
  assert.match(FakeEventSource.instances[2].url, /after=3$/);
  clock += 151000;
  timers.run();
  assert.match(errors[0], /Lost contact/);
  assert.ok(FakeEventSource.instances.at(-1).closed);
  assert.equal(timers.tasks.size, 0, 'no timers left behind');
});

test('relay frames are posted with the seat token and an abort signal, and API errors are returned', async () => {
  const requests = [];
  const client = new RoomClient({ room: roomId, token, seat: 0, mode: 'relay' }, async (path, options) => {
    requests.push({ path, options });
    return response({ error: 'gap', code: 'gap', expected: { 1: 2 } }, 422);
  }, FakeEventSource);
  const controller = new AbortController();
  const result = await client.sendFrames({ frames: [{ to: 1, seq: 3, payload: 'AAAA' }], ack: 4 }, controller.signal);
  assert.deepEqual(result, { status: 422, data: { error: 'gap', code: 'gap', expected: { 1: 2 } } });
  assert.equal(requests[0].path, `/api/rooms/${roomId}/relay`);
  assert.equal(requests[0].options.headers.Authorization, `Bearer ${token}`);
  assert.equal(requests[0].options.signal, controller.signal);
  assert.deepEqual(JSON.parse(requests[0].options.body), { frames: [{ to: 1, seq: 3, payload: 'AAAA' }], ack: 4 });
});

test('names are limited to 40 code points, as the server counts them', () => {
  const emoji = '\u{1F0CF}'; // one code point, two UTF-16 units
  assert.equal(validatePlayers([{ ...players[0], name: emoji.repeat(40) }])[0].name, emoji.repeat(40));
  assert.throws(() => validatePlayers([{ ...players[0], name: emoji.repeat(41) }]), /invalid player/);
  assert.deepEqual(validateNames([emoji.repeat(40), 'Bert', 'Clara']), [emoji.repeat(40), 'Bert', 'Clara']);
  assert.throws(() => validateNames([emoji.repeat(41), 'Bert', 'Clara']), /1–40 characters/);
});

test('a relay seat sends ack-only keepalives when nothing else was posted for a while', async () => {
  FakeEventSource.instances = [];
  const timers = new FakeTimers();
  const posts = [];
  const fetcher = async (path, options) => { posts.push({ path, body: JSON.parse(options.body) }); return response({ expected: {} }); };
  const client = new RoomClient({ room: roomId, token, seat: 0, mode: 'relay' }, fetcher, FakeEventSource, timers, 30000);
  let clock = 0;
  client.now = () => clock;
  client.lastPost = 0;
  client.listen(() => {}, assert.fail);
  FakeEventSource.instances[0].onmessage({ data: JSON.stringify({ type: 'relay', from: 1, seq: 1, payload: 'AAAA' }), lastEventId: '5' });
  clock = 29000;
  timers.run();
  assert.equal(posts.length, 0);
  clock = 30000;
  timers.run();
  await settleMicrotasks();
  assert.deepEqual(posts, [{ path: `/api/rooms/${roomId}/relay`, body: { frames: [], ack: 5 } }]);
  // Uploads count as keepalives too.
  clock = 50000;
  await client.sendFrames({ frames: [{ to: 1, seq: 2, payload: 'AAAA' }], ack: 5 });
  clock = 79000;
  timers.run();
  await settleMicrotasks();
  assert.equal(posts.length, 2);
  clock = 80000;
  timers.run();
  await settleMicrotasks();
  assert.equal(posts.length, 3);
  client.disconnectEvents();
});

test('after a server restart the relay stream fails at once instead of retrying for minutes', async () => {
  for (const [status, code, fails] of [[404, 'not_found', true], [401, 'unauthorized', true], [404, 'room_closed', false], [0, 'network', false]]) {
    FakeEventSource.instances = [];
    const timers = new FakeTimers();
    const fetcher = async () => {
      if (status === 0) throw new TypeError('Failed to fetch');
      return response({ error: 'gone', code }, status);
    };
    const client = new RoomClient({ room: roomId, token, seat: 0, mode: 'relay' }, fetcher, FakeEventSource, timers);
    const errors = [];
    client.listen(() => {}, error => errors.push(error.message));
    const source = FakeEventSource.instances[0];
    source.readyState = 2; // EventSource gave up: the server answered with an error status
    source.onerror();
    await settleMicrotasks();
    assert.deepEqual(errors, fails ? ['The table server restarted or closed the table.'] : [], `${status} ${code}`);
    client.disconnectEvents();
  }
});
