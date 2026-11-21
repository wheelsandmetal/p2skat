import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { MAX_MESSAGE_BYTES, MessageQueue, MeshReadiness, checkText, parseSignal, iceConfiguration, PeerMesh } from './transport.mjs';

const names = ['Ada', 'Bob', 'Cleo'];
const packet = (seat, type, fields = {}) => ({ version: 1, seat, type, ...fields });

test('static controls have unique DOM ids so network inputs cannot shadow table state', () => {
  const html = readFileSync(new URL('./index.html', import.meta.url), 'utf8');
  const ids = [...html.matchAll(/\bid="([^"]+)"/g)].map(match => match[1]);
  assert.equal(new Set(ids).size, ids.length);
  assert.ok(ids.includes('turn-url'));
});

test('messages received before Wasm is ready are retained and delivered in order', () => {
  const queue = new MessageQueue();
  const delivered = [];
  queue.push(1, '{"message":1}');
  queue.push(2, '{"message":2}');
  assert.equal(delivered.length, 0);
  queue.start((seat, json) => delivered.push([seat, JSON.parse(json).message]));
  queue.push(1, '{"message":3}');
  assert.deepEqual(delivered, [[1, 1], [2, 2], [1, 3]]);
  assert.equal(queue.bytes, 0);
  queue.clear();
  queue.push(2, '{"message":4}');
  assert.equal(delivered.length, 3);
});

test('early message queue bounds memory and payloads by UTF-8 bytes', () => {
  const queue = new MessageQueue();
  for (let i = 0; i < 256; i++) queue.push(1, '{}');
  assert.throws(() => queue.push(1, '{}'), /Too many/);
  assert.throws(() => checkText('♠'.repeat(MAX_MESSAGE_BYTES / 2)), /too large/);
  assert.throws(() => checkText(new ArrayBuffer(4)), /invalid/);
});

test('full mesh requires both peers to identify themselves and report both connections ready', () => {
  const mesh = new MeshReadiness(0, names);
  assert.equal(mesh.receive(1, packet(1, 'hello', { names })), false);
  assert.equal(mesh.receive(1, packet(1, 'ready')), false);
  assert.equal(mesh.receive(2, packet(2, 'hello', { names })), false);
  assert.equal(mesh.receive(2, packet(2, 'ready')), true);
  assert.equal(mesh.receive(1, packet(1, 'game', { body: '{"Bid":18}' })), true);
});

test('readiness rejects forged seats, mismatched names, premature game messages and unknown messages', () => {
  const mesh = new MeshReadiness(0, names);
  assert.throws(() => mesh.receive(1, packet(2, 'hello', { names })), /invalid seat/);
  assert.throws(() => mesh.receive(0, packet(0, 'hello', { names })), /invalid seat/);
  assert.throws(() => mesh.receive(1, packet(1, 'ready')), /before identifying/);
  assert.throws(() => mesh.receive(1, packet(1, 'hello', { names: ['X', 'Bob', 'Cleo'] })), /disagree/);
  mesh.receive(1, packet(1, 'hello', { names }));
  assert.throws(() => mesh.receive(1, packet(1, 'hello', { names })), /repeated/);
  assert.throws(() => mesh.receive(1, packet(1, 'game', { body: '{}' })), /before the table/);
  assert.throws(() => mesh.receive(1, packet(1, 'surprise')), /Unknown/);
});

test('manual signaling binds each offer and answer to the correct fixed seats and names', () => {
  const offer = { version: 1, from: 0, to: 2, names, type: 'offer', sdp: 'v=0\r\nexample' };
  assert.deepEqual(parseSignal(JSON.stringify(offer), 2, 0, names), { type: 'offer', sdp: offer.sdp });
  assert.throws(() => parseSignal(JSON.stringify(offer), 1, 0, names), /do not match/);
  assert.throws(() => parseSignal(JSON.stringify({ ...offer, type: 'answer' }), 2, 0, names), /do not match/);
  assert.throws(() => parseSignal(JSON.stringify({ ...offer, names: [...names].reverse() }), 2, 0, names), /do not match/);
  const answer = { ...offer, from: 2, to: 0, type: 'answer' };
  assert.equal(parseSignal(JSON.stringify(answer), 0, 2, names).type, 'answer');
  assert.throws(() => parseSignal('x'.repeat(MAX_MESSAGE_BYTES + 1), 2, 0, names), /too large/);
});

test('TURN credentials stay client-supplied and relay-only mode requires TURN', () => {
  assert.throws(() => iceConfiguration({ stun: '', turn: '', relayOnly: true }), /requires a TURN/);
  assert.throws(() => iceConfiguration({ stun: 'https://example.com', turn: '' }), /valid stun/);
  assert.throws(() => iceConfiguration({ stun: '', turn: 'turn:example.com' }), /temporary username/);
  assert.deepEqual(iceConfiguration({ stun: '', turn: 'turn:example.com:3478', username: 'temporary', credential: 'short-lived', relayOnly: true }), {
    iceServers: [{ urls: 'turn:example.com:3478', username: 'temporary', credential: 'short-lived' }], iceTransportPolicy: 'relay',
  });
});

test('send respects negotiated SCTP message limits and refuses a backed-up channel', () => {
  const sent = [];
  const mesh = Object.create(PeerMesh.prototype);
  mesh.seat = 0;
  mesh.started = true;
  mesh.closed = false;
  const channel = { readyState: 'open', bufferedAmount: 0, send: text => sent.push(text) };
  const connection = { sctp: { maxMessageSize: 128 } };
  mesh.peers = new Map([[1, { connection, channel }]]);
  mesh.send(1, '{}');
  assert.equal(JSON.parse(sent[0]).seat, 0);
  assert.throws(() => mesh.send(1, 'x'.repeat(100)), /too large/);
  channel.bufferedAmount = 2 * 1024 * 1024;
  assert.throws(() => mesh.send(1, '{}'), /too far behind/);
  channel.bufferedAmount = 0;
  channel.readyState = 'closed';
  assert.throws(() => mesh.send(1, '{}'), /not open/);
});

test('three real mesh handlers start only after all pairings, then propagate disconnect to everyone', async () => {
  const started = [];
  const failed = [];
  const received = [];
  const meshes = [0, 1, 2].map(seat => Object.assign(Object.create(PeerMesh.prototype), {
    seat, names, readiness: new MeshReadiness(seat, names), peers: new Map(),
    closed: false, announced: false, started: false,
    onReady: () => started.push(seat), onStatus: () => {},
    onFailure: error => failed.push([seat, error.message]),
    onMessage: (remote, text) => received.push([seat, remote, text]),
  }));
  const pairs = [[0, 1], [0, 2], [1, 2]].map(([first, second]) => {
    const channels = [first, second].map(() => ({
      label: 'p2skat', ordered: true, maxRetransmits: null, maxPacketLifeTime: null,
      readyState: 'connecting', bufferedAmount: 0,
    }));
    channels.forEach((channel, i) => {
      const other = channels[1 - i];
      channel.send = text => queueMicrotask(() => other.onmessage?.({ data: text }));
      channel.close = () => {
        if (channel.readyState === 'closed') return;
        channel.readyState = other.readyState = 'closed';
        queueMicrotask(() => { channel.onclose?.(); other.onclose?.(); });
      };
      const local = i ? second : first;
      const remote = i ? first : second;
      meshes[local].peers.set(remote, { channel: null, connection: { sctp: { maxMessageSize: 65536 }, close: channel.close } });
      meshes[local].attach(remote, channel);
    });
    return {
      open: () => { channels.forEach(channel => { channel.readyState = 'open'; }); channels.forEach(channel => channel.onopen()); },
      close: channels[0].close,
    };
  });
  pairs[0].open();
  pairs[1].open();
  await new Promise(setImmediate);
  assert.deepEqual(started, [], 'One player having two connections is insufficient');
  pairs[2].open();
  await new Promise(setImmediate);
  assert.deepEqual([...started].sort(), [0, 1, 2]);
  meshes[0].send(2, '{"test":true}');
  await new Promise(setImmediate);
  assert.deepEqual(received, [[2, 0, '{"test":true}']]);
  pairs[0].close();
  await new Promise(setImmediate);
  assert.ok(meshes.every(mesh => mesh.closed));
  assert.equal(failed.length, 3);
});
