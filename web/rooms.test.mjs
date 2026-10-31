import test from 'node:test';
import assert from 'node:assert/strict';
import { RoomClient, browserMarker, parseRoomEvent, validatePlayers } from './rooms.mjs';

const roomId = 'unguessable-room-id';
const token = 'unguessable-seat-token';
const players = [{ seat: 0, name: 'Ada', browser: 'browser-identifier-1' }];
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
