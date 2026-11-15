import test from 'node:test';
import assert from 'node:assert/strict';
import { PeerMesh } from './transport.mjs';
import {
  RelaySession, RelayUploader, TAMPERED_MESSAGE, buildKeyFrame, canonical, decodeBase64Url, deriveAuthKey,
  encodeBase64Url, newInviteSecret, parseInviteSecret, relayInviteFragment,
} from './relay.mjs';

const names = ['Ada', 'Bert', 'Clara'];
const roomId = 'unguessable-relay-room-id';
const { subtle } = globalThis.crypto;
const settle = (ms = 0) => new Promise(resolve => setTimeout(resolve, ms));
const decode = payload => new TextDecoder().decode(decodeBase64Url(payload));

// An in-memory table server with the relay semantics of scripts/serve.py: per (from, to) dedup,
// gap rejection and `expected`. Delivery to each seat is asynchronous and in order.
class FakeRelay {
  constructor({ deliver = true } = {}) {
    this.deliver = deliver;
    this.last = new Map();
    this.sessions = [];
    this.posts = [];
    this.forwarded = [];
    this.inFlight = 0;
    this.maxInFlight = 0;
  }

  expected(from) {
    return Object.fromEntries([0, 1, 2].filter(seat => seat !== from).map(seat => [String(seat), (this.last.get(`${from}>${seat}`) || 0) + 1]));
  }

  post(from) {
    return async body => {
      this.posts.push({ from, body: structuredClone(body) });
      this.inFlight++;
      this.maxInFlight = Math.max(this.maxInFlight, this.inFlight);
      await settle();
      this.inFlight--;
      const accepted = [];
      const last = new Map(this.last);
      for (const { to, seq, payload } of body.frames) {
        const previous = last.get(`${from}>${to}`) || 0;
        if (seq === previous + 1) {
          last.set(`${from}>${to}`, seq);
          accepted.push({ type: 'relay', to, from, seq, payload });
        } else if (seq > previous + 1) return { status: 422, data: { error: 'gap', code: 'gap', expected: this.expected(from) } };
      }
      this.last = last;
      for (const event of accepted) {
        this.forwarded.push(event);
        if (this.deliver) setTimeout(() => this.sessions[event.to]?.receive(event), 0);
      }
      return { status: 200, data: { expected: this.expected(from) } };
    };
  }
}

function makeTable({ relay = new FakeRelay(), tableNames = [names, names, names], crypto = globalThis.crypto,
  secret = newInviteSecret(), uploader = {} } = {}) {
  const failures = [];
  const sessions = [0, 1, 2].map(seat => new RelaySession({ roomId, seat, names: tableNames[seat], secret,
    post: relay.post(seat), crypto, uploader, onFatal: error => failures.push([seat, error.message]) }));
  relay.sessions = sessions;
  return { relay, sessions, failures, secret };
}

function collect(session, remote) {
  const messages = [];
  session.channel(remote).onmessage = ({ data }) => messages.push(data);
  return messages;
}

async function openTable(options) {
  const table = makeTable(options);
  table.sessions.forEach(session => session.start());
  await waitFor(() => table.sessions.every(session => [...session.pairs.values()].every(pair => pair.channel.readyState === 'open')) || table.failures.length);
  return table;
}

async function waitFor(predicate, timeout = 3000) {
  const deadline = Date.now() + timeout;
  while (!predicate()) {
    if (Date.now() > deadline) throw new Error('Timed out');
    await settle(2);
  }
}

test('invite secrets are 32 random bytes in strict base64url, and a missing key is reported', () => {
  const secret = newInviteSecret();
  const fragment = relayInviteFragment(roomId, secret);
  const params = new URLSearchParams(fragment);
  assert.equal(params.get('room'), roomId);
  assert.equal(params.get('m'), 'relay');
  assert.deepEqual(parseInviteSecret(params.get('k')), secret);
  assert.throws(() => parseInviteSecret(null), /incomplete/);
  assert.throws(() => parseInviteSecret(params.get('k').slice(0, 20)), /incomplete/);
  assert.throws(() => parseInviteSecret(`${params.get('k')}=`), /incomplete/);
  assert.equal(decodeBase64Url('A'), null);
  assert.equal(decodeBase64Url('AB'), null, 'non-canonical trailing bits are rejected');
});

test('canonical encoding is length-prefixed, so moving a separator changes the MAC input', () => {
  assert.notDeepEqual(canonical(['Ada|', 'Bert']), canonical(['Ada', '|Bert']));
  assert.notDeepEqual(canonical(['ab', 'c']), canonical(['a', 'bc']));
  assert.deepEqual([...canonical(['é'])], [0, 0, 0, 2, 0xc3, 0xa9]);
});

test('three seats exchange keys, then deliver packets in order on every pair', async () => {
  const { sessions, failures } = await openTable();
  assert.deepEqual(failures, []);
  const received = collect(sessions[2], 0);
  const back = collect(sessions[0], 2);
  for (let i = 0; i < 20; i++) sessions[0].channel(2).send(`{"n":${i}}`);
  sessions[2].channel(0).send('{"reply":true}');
  await waitFor(() => received.length === 20 && back.length === 1);
  assert.deepEqual(received, Array.from({ length: 20 }, (_, i) => `{"n":${i}}`));
  await waitFor(() => sessions[0].channel(2).bufferedAmount === 0);
  sessions.forEach(session => session.close());
});

test('the relay never sees plaintext: only key frames are readable and they hold no packet', async () => {
  const { relay, sessions } = await openTable();
  const received = collect(sessions[1], 0);
  sessions[0].channel(1).send(JSON.stringify({ type: 'game', body: '{"secret":"Grand"}', version: 1, seat: 0 }));
  await waitFor(() => received.length === 1);
  for (const { seq, payload } of relay.forwarded) {
    const bytes = decodeBase64Url(payload);
    const text = new TextDecoder().decode(bytes);
    assert.doesNotMatch(text, /"type":"game"|Grand|hello/);
    if (seq === 1) assert.equal(JSON.parse(text).type, 'key');
  }
  sessions.forEach(session => session.close());
});

test('a tampered key-frame MAC fails with the tampered-invite message', async () => {
  const relay = new FakeRelay({ deliver: false });
  const { sessions, failures } = makeTable({ relay });
  sessions.forEach(session => session.start());
  await waitFor(() => relay.forwarded.filter(event => event.seq === 1).length === 6);
  const frame = relay.forwarded.find(event => event.from === 0 && event.to === 1);
  const parsed = JSON.parse(decode(frame.payload));
  const mac = decodeBase64Url(parsed.mac);
  mac[0] ^= 1;
  parsed.mac = encodeBase64Url(mac);
  sessions[1].receive({ from: 0, seq: 1, payload: encodeBase64Url(new TextEncoder().encode(JSON.stringify(parsed))) });
  await waitFor(() => failures.length);
  assert.deepEqual(failures, [[1, TAMPERED_MESSAGE]]);
});

test('a substituted public key fails the MAC', async () => {
  const relay = new FakeRelay({ deliver: false });
  const { sessions, failures } = makeTable({ relay });
  sessions.forEach(session => session.start());
  await waitFor(() => relay.forwarded.filter(event => event.seq === 1).length === 6);
  const honest = JSON.parse(decode(relay.forwarded.find(event => event.from === 0 && event.to === 1).payload));
  const other = JSON.parse(decode(relay.forwarded.find(event => event.from === 2 && event.to === 1).payload));
  honest.pub = other.pub;
  sessions[1].receive({ from: 0, seq: 1, payload: encodeBase64Url(new TextEncoder().encode(JSON.stringify(honest))) });
  await waitFor(() => failures.length);
  assert.deepEqual(failures, [[1, TAMPERED_MESSAGE]]);
});

test('players who were shown different names refuse each other', async () => {
  const { failures } = await openTable({ tableNames: [names, ['Ada', 'Bert', 'Mallory'], names] });
  await waitFor(() => failures.length >= 2);
  assert.ok(failures.every(([, message]) => message === TAMPERED_MESSAGE));
});

test('valid MACs for another mode, room, or seat pairing are still refused', async t => {
  const secret = newInviteSecret();
  const authKey = await deriveAuthKey(subtle, secret, roomId);
  const keyPair = await subtle.generateKey({ name: 'ECDH', namedCurve: 'P-256' }, false, ['deriveBits']);
  const pub = new Uint8Array(await subtle.exportKey('raw', keyPair.publicKey));
  const honest = { from: 0, to: 1, room_id: roomId, mode: 'relay', names, pub };
  for (const [label, fields] of [
    ['mode', { ...honest, mode: 'webrtc' }],
    ['room', { ...honest, room_id: 'another-relay-room-id' }],
    ['destination', { ...honest, to: 2 }],
    ['sender', { ...honest, from: 2 }],
  ]) {
    await t.test(label, async () => {
      const failures = [];
      const session = new RelaySession({ roomId, seat: 1, names, secret, post: async () => ({ status: 200, data: { expected: {} } }),
        onFatal: error => failures.push(error.message) });
      const key = label === 'room' ? await deriveAuthKey(subtle, secret, fields.room_id) : authKey;
      session.receive({ from: 0, seq: 1, payload: await buildKeyFrame(subtle, key, fields) });
      await waitFor(() => failures.length);
      assert.deepEqual(failures, [TAMPERED_MESSAGE]);
    });
  }
  const failures = [];
  const session = new RelaySession({ roomId, seat: 1, names, secret, post: async () => ({ status: 200, data: { expected: {} } }),
    onFatal: error => failures.push(error.message) });
  session.receive({ from: 0, seq: 1, payload: await buildKeyFrame(subtle, authKey, honest) });
  await waitFor(() => session.channel(0).readyState === 'open');
  assert.deepEqual(failures, [], 'the same frame with matching fields is accepted');
  session.close();
});

test('duplicate frames are dropped silently, gaps are fatal', async () => {
  const { relay, sessions, failures } = await openTable();
  const received = collect(sessions[1], 0);
  sessions[0].channel(1).send('{"n":1}');
  await waitFor(() => received.length === 1);
  const frame = relay.forwarded.find(event => event.from === 0 && event.to === 1 && event.seq === 2);
  sessions[1].receive(frame);
  sessions[1].receive(relay.forwarded.find(event => event.from === 0 && event.to === 1 && event.seq === 1));
  await settle(20);
  assert.deepEqual(received, ['{"n":1}']);
  assert.deepEqual(failures, []);
  sessions[1].receive({ ...frame, seq: 4 });
  await waitFor(() => failures.length);
  assert.match(failures[0][1], /Ada were lost or reordered/);
});

test('a frame that fails AEAD authentication is fatal', async () => {
  const relay = new FakeRelay();
  const { sessions, failures } = await openTable({ relay });
  relay.deliver = false;
  sessions[0].channel(1).send('{"n":1}');
  await waitFor(() => relay.forwarded.some(event => event.from === 0 && event.to === 1 && event.seq === 2));
  const frame = relay.forwarded.find(event => event.from === 0 && event.to === 1 && event.seq === 2);
  const bytes = decodeBase64Url(frame.payload);
  bytes[3] ^= 0x40;
  sessions[1].receive({ ...frame, payload: encodeBase64Url(bytes) });
  await waitFor(() => failures.length);
  assert.match(failures[0][1], /failed its integrity check/);
});

test('the third player, holding the invite secret, cannot read or forge a pair\'s frames', async () => {
  const relay = new FakeRelay();
  const { sessions, failures } = await openTable({ relay });
  relay.deliver = false;
  sessions[0].channel(1).send('{"private":true}');
  await waitFor(() => relay.forwarded.some(event => event.from === 0 && event.to === 1 && event.seq === 2));
  const frame = relay.forwarded.find(event => event.from === 0 && event.to === 1 && event.seq === 2);
  // Seat 2 tries the frame under its own keys with seat 0 (the server could stamp any `from`).
  sessions[2].receive({ ...frame, to: 2 });
  await waitFor(() => failures.length);
  assert.deepEqual(failures.map(([seat]) => seat), [2]);
  assert.match(failures[0][1], /integrity check/);
});

test('send order survives encryption that completes out of order', async () => {
  const slowSubtle = new Proxy(subtle, {
    get(target, key) {
      if (key === 'encrypt') return async (...args) => { await settle(Math.random() * 8); return target.encrypt(...args); };
      const value = target[key];
      return typeof value === 'function' ? value.bind(target) : value;
    },
  });
  const crypto = { subtle: slowSubtle, getRandomValues: array => globalThis.crypto.getRandomValues(array) };
  const { sessions, failures } = await openTable({ crypto });
  const received = collect(sessions[1], 0);
  const sent = Array.from({ length: 30 }, (_, i) => `{"i":${i}}`);
  sent.forEach(text => sessions[0].channel(1).send(text));
  await waitFor(() => received.length === sent.length);
  assert.deepEqual(received, sent);
  assert.deepEqual(failures, []);
  sessions.forEach(session => session.close());
});

test('send() is synchronous, fixes seq up front, and refuses to buffer without bound', async () => {
  const { sessions } = await openTable();
  const pair = sessions[0].pairs.get(1);
  const before = pair.sendSeq;
  sessions[0].channel(1).send('{}');
  assert.equal(pair.sendSeq, before + 1, 'seq assigned before any await');
  assert.equal(sessions[0].channel(1).bufferedAmount, 2);
  sessions[0].channel(1).bufferedAmount = 1024 * 1024;
  assert.throws(() => sessions[0].channel(1).send('{}'), /too far behind/);
  sessions.forEach(session => session.close());
  assert.throws(() => sessions[0].channel(1).send('{}'), /not open/);
});

test('three PeerMesh instances become ready over relay channels and exchange game packets', async () => {
  const { sessions, failures } = makeTable();
  const ready = [];
  const received = [];
  const meshes = sessions.map((relay, seat) => new PeerMesh({ seat, names, relay,
    relayPeers: [0, 1, 2].filter(other => other !== seat),
    onSignal: () => assert.fail('relay peers exchange no signals'), onStatus: () => {},
    onReady: () => ready.push(seat), onMessage: (remote, json) => received.push([seat, remote, json]),
    onFailure: error => failures.push([seat, error.message]) }));
  await Promise.all(meshes.map(mesh => mesh.prepare()));
  sessions.forEach(session => session.start());
  await waitFor(() => ready.length === 3);
  meshes[2].send(0, '{"PlayCard":1}');
  await waitFor(() => received.length === 1);
  assert.deepEqual(received, [[0, 2, '{"PlayCard":1}']]);
  assert.deepEqual(failures, []);
  meshes.forEach(mesh => mesh.close());
  sessions.forEach(session => session.close());
});

// Uploader --------------------------------------------------------------------

function manualPost() {
  const calls = [];
  const post = (body, signal) => new Promise((resolve, reject) => {
    calls.push({ body: structuredClone(body), signal, resolve, reject });
    signal.addEventListener('abort', () => reject(Object.assign(new Error('aborted'), { name: 'AbortError' })));
  });
  return { calls, post };
}
const frame = (to, seq) => ({ to, seq, payload: `payload-${to}-${seq}`, bytes: 1 });
const ok = expected => ({ status: 200, data: { expected } });

test('one POST in flight; everything queued meanwhile goes in the next batch with the latest ack', async () => {
  const { calls, post } = manualPost();
  let ack = 3;
  const confirmed = [];
  const uploader = new RelayUploader({ lingerMs: 0, post, getAck: () => ack, onFatal: assert.fail, onConfirmed: f => confirmed.push(`${f.to}:${f.seq}`) });
  uploader.enqueue(frame(1, 1));
  uploader.enqueue(frame(1, 2));
  uploader.enqueue(frame(2, 1));
  assert.equal(calls.length, 1);
  assert.deepEqual(calls[0].body, { frames: [{ to: 1, seq: 1, payload: 'payload-1-1' }], ack: 3 });
  ack = 7;
  calls[0].resolve(ok({ 1: 2, 2: 1 }));
  await settle();
  assert.equal(calls.length, 2);
  assert.deepEqual(calls[1].body.frames.map(f => [f.to, f.seq]), [[1, 2], [2, 1]]);
  assert.equal(calls[1].body.ack, 7);
  uploader.enqueue(frame(1, 3));
  uploader.enqueue(frame(2, 2));
  assert.equal(calls.length, 2, 'still one in flight');
  calls[1].resolve(ok({ 1: 3, 2: 2 }));
  await settle();
  assert.deepEqual(calls[2].body.frames.map(f => [f.to, f.seq]), [[1, 3], [2, 2]]);
  calls[2].resolve(ok({ 1: 4, 2: 3 }));
  await settle();
  assert.equal(calls.length, 3);
  assert.deepEqual(confirmed, ['1:1', '1:2', '2:1', '1:3', '2:2']);
  assert.equal(uploader.queue.length, 0);
});

test('an idle uploader lingers briefly so a burst of sends shares one POST', async () => {
  const { calls, post } = manualPost();
  const uploader = new RelayUploader({ post, onFatal: assert.fail, lingerMs: 5 });
  uploader.enqueue(frame(1, 1));
  await settle();
  uploader.enqueue(frame(2, 1));
  assert.equal(calls.length, 0);
  await waitFor(() => calls.length === 1);
  assert.deepEqual(calls[0].body.frames.map(f => [f.to, f.seq]), [[1, 1], [2, 1]]);
  uploader.enqueue(frame(1, 2));
  calls[0].resolve(ok({ 1: 2, 2: 2 }));
  await settle();
  assert.equal(calls.length, 2, 'frames that waited behind a POST go at once');
  uploader.close();
});

test('a 200 reply that does not confirm a sent frame is fatal', async () => {
  const { calls, post } = manualPost();
  const failures = [];
  const uploader = new RelayUploader({ lingerMs: 0, post, onFatal: error => failures.push(error.message) });
  uploader.enqueue(frame(1, 1));
  calls[0].resolve(ok({ 1: 1, 2: 1 }));
  await waitFor(() => failures.length);
  assert.match(failures[0], /did not accept/);
});

test('failed and timed-out POSTs are retried with identical ciphertext', async () => {
  const { calls, post } = manualPost();
  const uploader = new RelayUploader({ lingerMs: 0, post, onFatal: assert.fail, backoffMs: [1], timeoutMs: 20 });
  uploader.enqueue(frame(1, 1));
  calls[0].reject(new TypeError('Failed to fetch'));
  await waitFor(() => calls.length === 2);
  uploader.enqueue(frame(1, 2));
  await waitFor(() => calls.length === 3, 1000); // the second attempt times out (AbortController)
  assert.ok(calls[1].signal.aborted);
  calls[2].resolve({ status: 503, data: { error: 'busy', code: 'busy' } });
  await waitFor(() => calls.length === 4);
  assert.deepEqual(calls[0].body.frames, calls[1].body.frames);
  assert.deepEqual(calls[2].body.frames.slice(0, 1), calls[0].body.frames);
  assert.deepEqual(calls[3].body.frames, calls[2].body.frames);
  calls[3].resolve(ok({ 1: 3 }));
  await settle();
  assert.equal(uploader.queue.length, 0);
});

test('a gap reply skips frames the server already holds and fails if it lost any', async () => {
  const { calls, post } = manualPost();
  const failures = [];
  const uploader = new RelayUploader({ lingerMs: 0, post, onFatal: error => failures.push(error.message), backoffMs: [1], timeoutMs: 1000 });
  uploader.enqueue(frame(1, 1));
  uploader.enqueue(frame(1, 2));
  uploader.enqueue(frame(1, 3));
  calls[0].reject(new TypeError('dropped')); // it carried only seq 1
  await waitFor(() => calls.length === 2);
  // Suppose the dropped POST had applied seq 1 and 2: the server skips them, so no gap. A gap reply
  // that says it already has 1–2 lets the client resend from 3 instead.
  calls[1].resolve({ status: 422, data: { error: 'gap', code: 'gap', expected: { 1: 3, 2: 1 } } });
  await waitFor(() => calls.length === 3);
  assert.deepEqual(calls[2].body.frames.map(f => f.seq), [3]);
  // A server that claims to need seq 2 again after confirming it has lost a frame for good.
  calls[2].resolve({ status: 422, data: { error: 'gap', code: 'gap', expected: { 1: 2, 2: 1 } } });
  await waitFor(() => failures.length);
  assert.match(failures[0], /lost messages/);
});

test('terminal server errors end the table with the server message; a closed table waits for its reason', async () => {
  const { calls, post } = manualPost();
  const failures = [];
  const uploader = new RelayUploader({ lingerMs: 0, post, onFatal: error => failures.push(error.message), closedGraceMs: 10 });
  uploader.enqueue(frame(1, 1));
  calls[0].resolve({ status: 429, data: { error: 'Ada sent messages too quickly. Create a new table.', code: 'over_limit' } });
  await waitFor(() => failures.length);
  assert.deepEqual(failures, ['Ada sent messages too quickly. Create a new table.']);
  const second = manualPost();
  const later = [];
  const closing = new RelayUploader({ lingerMs: 0, post: second.post, onFatal: error => later.push(error.message), closedGraceMs: 10 });
  closing.enqueue(frame(1, 1));
  second.calls[0].resolve({ status: 404, data: { error: 'This table ended or expired', code: 'room_closed' } });
  await settle(1);
  assert.deepEqual(later, []);
  await waitFor(() => later.length);
  assert.deepEqual(later, ['This table has ended.']);
});

test('the uploader gives up after a long outage, and sends ack-only POSTs on request', async () => {
  let clock = 0;
  const failures = [];
  const calls = [];
  const uploader = new RelayUploader({ lingerMs: 0, post: async body => { calls.push(body); throw new TypeError('offline'); },
    getAck: () => 40, onFatal: error => failures.push(error.message), backoffMs: [1], giveUpMs: 100, now: () => clock });
  uploader.requestAck();
  assert.deepEqual(calls, [{ frames: [], ack: 40 }]);
  await waitFor(() => calls.length >= 2);
  clock = 1000;
  await waitFor(() => failures.length);
  assert.match(failures[0], /Lost contact/);
});
