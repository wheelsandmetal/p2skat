import test from 'node:test';
import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import { LocalDataChannel, PeerMesh } from './transport.mjs';

test('local channels connect when the second tab arrives, preserve order, and propagate leave', { timeout: 5000 }, async () => {
  const room = randomUUID();
  const first = new LocalDataChannel(room, 0, 1);
  let second;
  try {
    await new Promise(resolve => setTimeout(resolve, 20));
    assert.equal(first.readyState, 'connecting');
    const firstOpen = new Promise(resolve => { first.onopen = resolve; });
    second = new LocalDataChannel(room, 1, 0);
    const secondOpen = new Promise(resolve => { second.onopen = resolve; });
    await Promise.all([firstOpen, secondOpen]);
    const messages = [];
    const received = new Promise(resolve => { second.onmessage = event => {
      messages.push(event.data);
      if (messages.length === 2) resolve();
    }; });
    first.send('one');
    first.send('two');
    await received;
    assert.deepEqual(messages, ['one', 'two']);
    const closed = new Promise(resolve => { second.onclose = resolve; });
    first.close();
    await closed;
    assert.equal(second.readyState, 'closed');
  } finally { first.close(); second?.close(); }
});

test('three same-browser players become ready without any WebRTC objects or pasted signals', { timeout: 5000 }, async () => {
  const room = randomUUID();
  const meshes = [];
  const failures = [];
  try {
    const ready = [0, 1, 2].map(seat => new Promise(resolve => {
      meshes.push(new PeerMesh({ seat, names: ['Ada', 'Bert', 'Clara'], localRoom: room,
        localPeers: [0, 1, 2].filter(other => other !== seat),
        onSignal: () => assert.fail('Local tabs should not exchange SDP'),
        onStatus: () => {}, onReady: resolve, onMessage: () => {},
        onFailure: error => failures.push(error),
      }));
    }));
    await Promise.all(meshes.map(mesh => mesh.prepare()));
    await Promise.all(ready);
    assert.ok(meshes.every(mesh => mesh.started));
    assert.deepEqual(failures, []);
    const received = new Promise(resolve => { meshes[2].onMessage = (seat, text) => resolve([seat, text]); });
    meshes[0].send(2, '{"PlayCard":{"suit":0,"rank":0}}');
    assert.deepEqual(await received, [0, '{"PlayCard":{"suit":0,"rank":0}}']);
    meshes[0].close();
    await new Promise(resolve => setTimeout(resolve, 30));
    assert.ok(meshes.every(mesh => mesh.closed));
  } finally { meshes.forEach(mesh => mesh.close()); }
});
