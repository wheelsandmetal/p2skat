// End-to-end encrypted channels over the table server's relay (Tor Browser has no WebRTC).
//
// The server forwards opaque frames between seats and stamps the sender. Per pair of seats:
//   seq 1      a `key` frame: an ephemeral ECDH P-256 public key, bound by an HMAC under a
//              key derived from the invite secret to the room, both seats, the names and the mode;
//   seq 2, …   AES-GCM-256 frames carrying exactly the PeerMesh packet JSON.
// Receivers check the order themselves: duplicates are dropped, a gap or a failed
// integrity check ends the table. See docs/plans/tor-onion-relay.md, Phase 2.
import { checkText, validateNames } from './transport.mjs';
import { RESTARTED_MESSAGE } from './rooms.mjs';

export const RELAY_PROTOCOL = 'p2skat-relay-v1';
export const TAMPERED_MESSAGE = 'This invite link was tampered with or is for another table.';
export const INCOMPLETE_MESSAGE = 'This invite link is incomplete. Ask for the complete link again.';
export const MAX_SEQ = 2 ** 53 - 1;
// The server caps a decoded payload at 64 KiB + 64 bytes (plaintext cap + GCM tag fits).
export const MAX_PAYLOAD_CHARS = Math.ceil((64 * 1024 + 64) * 4 / 3);
const MAX_BUFFERED_BYTES = 1024 * 1024;
const ACK_EVERY = 32;
const ROOM_ID = /^[a-zA-Z0-9_-]{16,128}$/;
const BASE64URL = /^[A-Za-z0-9_-]*$/;
const encoder = new TextEncoder();
const decoder = new TextDecoder('utf-8', { fatal: true });
const utf8 = text => encoder.encode(text);

export function encodeBase64Url(bytes) {
  let binary = '';
  for (let i = 0; i < bytes.length; i += 0x8000) binary += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  return btoa(binary).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

// Strict: unpadded, canonical (re-encodes to the same text), or null.
export function decodeBase64Url(text) {
  if (typeof text !== 'string' || !BASE64URL.test(text) || text.length % 4 === 1) return null;
  let binary;
  try { binary = atob(text.replace(/-/g, '+').replace(/_/g, '/')); } catch { return null; }
  const bytes = Uint8Array.from(binary, char => char.charCodeAt(0));
  return encodeBase64Url(bytes) === text ? bytes : null;
}

export function newInviteSecret(crypto = globalThis.crypto) {
  return crypto.getRandomValues(new Uint8Array(32));
}

export function parseInviteSecret(text) {
  const secret = decodeBase64Url(text);
  if (!text || !secret || secret.length !== 32) throw new Error(INCOMPLETE_MESSAGE);
  return secret;
}

export function relayInviteFragment(roomId, secret) {
  return `room=${roomId}&k=${encodeBase64Url(secret)}&m=relay`;
}

// Length-prefixed (4-byte big-endian) concatenation: names are arbitrary Unicode, so no
// separator can be ambiguous.
export function canonical(parts) {
  const chunks = parts.map(part => typeof part === 'string' ? utf8(part) : part);
  const out = new Uint8Array(chunks.reduce((sum, chunk) => sum + 4 + chunk.length, 0));
  const view = new DataView(out.buffer);
  let offset = 0;
  for (const chunk of chunks) {
    view.setUint32(offset, chunk.length);
    out.set(chunk, offset + 4);
    offset += 4 + chunk.length;
  }
  return out;
}

export function canonicalKeyFrame({ from, to, room_id: roomId, mode, names, pub }) {
  return canonical([RELAY_PROTOCOL, 'key', String(from), String(to), roomId, mode,
    String(names.length), ...names, pub]);
}

export async function deriveAuthKey(subtle, secret, roomId) {
  const base = await subtle.importKey('raw', secret, 'HKDF', false, ['deriveKey']);
  return subtle.deriveKey({ name: 'HKDF', hash: 'SHA-256', salt: utf8(RELAY_PROTOCOL), info: utf8(`auth|${roomId}`) },
    base, { name: 'HMAC', hash: 'SHA-256', length: 256 }, false, ['sign', 'verify']);
}

// Returns the base64url payload of a key frame. Exported so tests can forge frames with valid MACs.
export async function buildKeyFrame(subtle, authKey, fields) {
  const mac = new Uint8Array(await subtle.sign('HMAC', authKey, canonicalKeyFrame(fields)));
  const frame = { type: 'key', from: fields.from, to: fields.to, room_id: fields.room_id, mode: fields.mode,
    names: fields.names, pub: encodeBase64Url(fields.pub), mac: encodeBase64Url(mac) };
  return encodeBase64Url(utf8(JSON.stringify(frame)));
}

const KEY_FIELDS = ['from', 'mac', 'mode', 'names', 'pub', 'room_id', 'to', 'type'];

// Parses and authenticates a key frame, then checks that it is meant for this exact pairing.
export async function verifyKeyFrame(subtle, authKey, payload, expected) {
  let frame, pub, mac;
  try {
    frame = JSON.parse(decoder.decode(decodeBase64Url(payload)));
    pub = decodeBase64Url(frame.pub);
    mac = decodeBase64Url(frame.mac);
  } catch { throw new Error(TAMPERED_MESSAGE); }
  if (!frame || typeof frame !== 'object' || Array.isArray(frame) ||
      JSON.stringify(Object.keys(frame).sort()) !== JSON.stringify(KEY_FIELDS) || frame.type !== 'key' ||
      !Number.isInteger(frame.from) || !Number.isInteger(frame.to) ||
      typeof frame.room_id !== 'string' || frame.room_id.length > 128 ||
      typeof frame.mode !== 'string' || frame.mode.length > 16 ||
      !Array.isArray(frame.names) || frame.names.length !== 3 ||
      frame.names.some(name => typeof name !== 'string' || [...name].length > 40) ||
      !pub || pub.length !== 65 || pub[0] !== 4 || !mac || mac.length !== 32) {
    throw new Error(TAMPERED_MESSAGE);
  }
  const fields = { ...frame, pub };
  if (!await subtle.verify('HMAC', authKey, mac, canonicalKeyFrame(fields)) ||
      frame.from !== expected.from || frame.to !== expected.to || frame.room_id !== expected.room_id ||
      frame.mode !== 'relay' || JSON.stringify(frame.names) !== JSON.stringify(expected.names)) {
    throw new Error(TAMPERED_MESSAGE);
  }
  return pub;
}

export async function deriveDirectionKeys(subtle, { privateKey, secret, roomId, seat, remote, ownPub, peerPub }) {
  let peerKey;
  try { peerKey = await subtle.importKey('raw', peerPub, { name: 'ECDH', namedCurve: 'P-256' }, false, []); }
  catch { throw new Error(TAMPERED_MESSAGE); }
  const bits = await subtle.deriveBits({ name: 'ECDH', public: peerKey }, privateKey, 256);
  const base = await subtle.importKey('raw', bits, 'HKDF', false, ['deriveKey']);
  const [lo, hi] = seat < remote ? [seat, remote] : [remote, seat];
  const pubLo = encodeBase64Url(seat === lo ? ownPub : peerPub);
  const pubHi = encodeBase64Url(seat === lo ? peerPub : ownPub);
  const direction = (from, to, usage) => subtle.deriveKey({
    name: 'HKDF', hash: 'SHA-256', salt: secret,
    info: utf8(`${RELAY_PROTOCOL}|${roomId}|${lo}|${hi}|${from}>${to}|${pubLo}|${pubHi}`),
  }, base, { name: 'AES-GCM', length: 256 }, false, [usage]);
  return { send: await direction(seat, remote, 'encrypt'), receive: await direction(remote, seat, 'decrypt') };
}

// Nonce = 4-byte direction tag ("p2", from, to) ‖ 8-byte big-endian seq. Keys are per direction,
// so the tag is defence in depth; seq never repeats within a direction.
export function frameNonce(from, to, seq) {
  const nonce = new Uint8Array(12);
  nonce.set([0x70, 0x32, from, to]);
  new DataView(nonce.buffer).setBigUint64(4, BigInt(seq));
  return nonce;
}

export function frameAad(roomId, from, to, seq) {
  return utf8(`${roomId}|${from}|${to}|${seq}`);
}

// Drains every pair's frames through one in-flight POST, batching whatever queued meanwhile.
// Frames stay queued until the server's `expected` confirms them; a retry resends the same
// frame objects, so the ciphertext under a given seq never changes. An idle uploader lingers
// for `lingerMs` first, so a burst of sends (e.g. the same deck to both peers) shares one POST
// instead of the second waiting a whole round trip behind the first.
export class RelayUploader {
  constructor({ post, getAck = () => 0, onFatal, onConfirmed = () => {}, timeoutMs = 30000,
    backoffMs = [250, 500, 1000, 2000, 4000, 8000], giveUpMs = 120000, closedGraceMs = 3000,
    maxBatchFrames = 32, maxBatchChars = 768 * 1024, lingerMs = 15, timers = globalThis, now = () => Date.now() }) {
    Object.assign(this, { post, getAck, onFatal, onConfirmed, timeoutMs, backoffMs, giveUpMs, closedGraceMs,
      maxBatchFrames, maxBatchChars, lingerMs, timers, now });
    this.queue = [];
    this.inFlight = null;
    this.timer = null;
    this.failures = 0;
    // When the current run of failed POSTs began; idle time before it is no outage.
    this.failingSince = null;
    this.ackSent = 0;
    this.ackWanted = false;
    this.closed = false;
  }

  enqueue(frame) {
    if (this.closed) return;
    this.queue.push(frame);
    this.pump();
  }

  requestAck() {
    this.ackWanted = true;
    this.pump();
  }

  // `now`: frames that already waited (behind a POST or a backoff) go without lingering.
  pump(now = false) {
    if (this.closed || this.inFlight || this.timer) return;
    if (!this.queue.length && !(this.ackWanted && this.getAck() > this.ackSent)) return;
    // Hidden tabs clamp timers to >= 1 s, so they send at once instead.
    if (!now && this.lingerMs > 0 && !globalThis.document?.hidden) {
      this.timer = this.timers.setTimeout(() => {
        this.timer = null;
        this.pump(true);
      }, this.lingerMs);
      return;
    }
    void this.flush();
  }

  async flush() {
    const batch = [];
    let chars = 0;
    for (const frame of this.queue) {
      if (batch.length && (batch.length >= this.maxBatchFrames || chars + frame.payload.length > this.maxBatchChars)) break;
      batch.push(frame);
      chars += frame.payload.length;
    }
    const ack = this.getAck();
    this.ackWanted = false;
    const controller = new AbortController();
    this.inFlight = controller;
    const timeout = this.timers.setTimeout(() => controller.abort(), this.timeoutMs);
    let result;
    let failure = null;
    try {
      result = await this.post({ frames: batch.map(({ to, seq, payload }) => ({ to, seq, payload })), ack }, controller.signal);
    } catch (error) { failure = error; }
    this.timers.clearTimeout(timeout);
    this.inFlight = null;
    if (this.closed) return;
    try {
      if (failure) {
        this.ackWanted = true;
        return this.retry();
      }
      const { status, data } = result || {};
      if (status === 200 && this.validExpected(data?.expected)) {
        this.confirm(data.expected);
        if (batch.some(frame => this.queue.includes(frame))) throw new Error('The table server did not accept a message. The table has ended.');
        this.ackSent = Math.max(this.ackSent, ack);
        this.failures = 0;
        this.failingSince = null;
        return this.pump(true);
      }
      if (status === 422 && data?.code === 'gap' && this.validExpected(data.expected)) {
        this.failures = 0;
        this.failingSince = null;
        // Only frames the server already has can be skipped; anything else it lost for good.
        const before = this.queue.length;
        this.confirm(data.expected);
        const first = new Map();
        for (const frame of this.queue) if (!first.has(frame.to)) first.set(frame.to, frame.seq);
        if (this.queue.length === before || [...first].some(([to, seq]) => data.expected[String(to)] !== seq)) {
          throw new Error('The table server lost messages. The table has ended.');
        }
        return this.pump(true);
      }
      if (status >= 500 || (status === 200 && !data) || status === undefined) return this.retry();
      if (status === 404 && (data?.code === 'room_closed' || data?.code === 'not_found')) {
        // The table closed; its `closed` event usually explains why, so give it a moment to arrive.
        // A table the server does not know at all (it restarted) never sends one.
        const message = data.code === 'not_found' ? RESTARTED_MESSAGE : 'This table has ended.';
        this.timer = this.timers.setTimeout(() => this.fail(new Error(message)), this.closedGraceMs);
        return;
      }
      throw new Error(typeof data?.error === 'string' && data.error.length <= 400 ? data.error : 'The table server rejected a message.');
    } catch (error) { this.fail(error); }
  }

  validExpected(expected) {
    return expected && typeof expected === 'object' &&
      Object.values(expected).every(value => Number.isSafeInteger(value) && value >= 1);
  }

  confirm(expected) {
    this.queue = this.queue.filter(frame => {
      const next = expected[String(frame.to)];
      if (Number.isSafeInteger(next) && frame.seq < next) {
        this.onConfirmed(frame);
        return false;
      }
      return true;
    });
  }

  retry() {
    this.failures++;
    this.failingSince ??= this.now();
    if (this.now() - this.failingSince > this.giveUpMs) {
      this.fail(new Error('Lost contact with the table server. The table has ended.'));
      return;
    }
    const base = this.backoffMs[Math.min(this.failures, this.backoffMs.length) - 1];
    this.timer = this.timers.setTimeout(() => {
      this.timer = null;
      this.pump(true);
    }, base * (0.75 + Math.random() * 0.5));
  }

  fail(error) {
    if (this.closed) return;
    this.close();
    this.onFatal(error);
  }

  close() {
    this.closed = true;
    this.timers.clearTimeout(this.timer);
    this.timer = null;
    this.inFlight?.abort();
  }
}

// Duck-typed like the RTCDataChannel surface PeerMesh.attach() uses.
export class RelayChannel {
  constructor(session, remote) {
    this.session = session;
    this.remote = remote;
    this.label = 'p2skat';
    this.ordered = true;
    this.maxRetransmits = this.maxPacketLifeTime = null;
    this.readyState = 'connecting';
    this.bufferedAmount = 0;
    this.onopen = this.onmessage = this.onerror = this.onclose = null;
  }

  open() {
    if (this.readyState !== 'connecting') return;
    this.readyState = 'open';
    this.onopen?.();
  }

  deliver(text) {
    if (this.readyState === 'open') this.onmessage?.({ data: text });
  }

  // Synchronous like RTCDataChannel.send: the seq is fixed before anything asynchronous happens.
  send(text) {
    if (this.readyState !== 'open') throw new Error('The relay connection is not open.');
    checkText(text);
    const bytes = encoder.encode(text).length;
    if (this.bufferedAmount + bytes > MAX_BUFFERED_BYTES) throw new Error('The network is too far behind to continue safely.');
    this.bufferedAmount += bytes;
    this.session.sendData(this.remote, text, bytes);
  }

  close(notify = true) {
    if (this.readyState === 'closed') return;
    this.readyState = 'closed';
    if (notify) this.onclose?.();
  }
}

export class RelaySession {
  constructor({ roomId, seat, names, secret, post, getAck = () => 0, onFatal = () => {},
    crypto = globalThis.crypto, uploader = {} }) {
    if (!ROOM_ID.test(roomId) || !Number.isInteger(seat) || seat < 0 || seat > 2 ||
        !(secret instanceof Uint8Array) || secret.length !== 32) {
      throw new Error('Invalid relay table details.');
    }
    this.roomId = roomId;
    this.seat = seat;
    this.names = validateNames(names);
    this.secret = secret;
    this.subtle = crypto.subtle;
    this.getAck = getAck;
    this.onFatal = onFatal;
    this.closed = false;
    this.started = false;
    this.pairs = new Map();
    for (let remote = 0; remote < 3; remote++) {
      if (remote === seat) continue;
      this.pairs.set(remote, { remote, channel: new RelayChannel(this, remote), keys: null,
        sendSeq: 0, sendChain: Promise.resolve(), receiveSeq: 0, receiveChain: Promise.resolve() });
    }
    this.uploader = new RelayUploader({ post, getAck, ...uploader,
      onFatal: error => this.fail(error),
      onConfirmed: frame => { this.pairs.get(frame.to).channel.bufferedAmount -= frame.bytes; } });
    this.setup = this.init();
    this.setup.catch(error => this.fail(error));
  }

  async init() {
    const { subtle } = this;
    const keyPair = await subtle.generateKey({ name: 'ECDH', namedCurve: 'P-256' }, false, ['deriveBits']);
    const pub = new Uint8Array(await subtle.exportKey('raw', keyPair.publicKey));
    const authKey = await deriveAuthKey(subtle, this.secret, this.roomId);
    return { privateKey: keyPair.privateKey, pub, authKey };
  }

  channel(remote) {
    return this.pairs.get(remote).channel;
  }

  // Sends this seat's key frame (seq 1) to both other seats.
  start() {
    if (this.started || this.closed) return;
    this.started = true;
    for (const pair of this.pairs.values()) {
      this.enqueue(pair, 0, async () => {
        const { pub, authKey } = await this.setup;
        return buildKeyFrame(this.subtle, authKey, { from: this.seat, to: pair.remote, room_id: this.roomId,
          mode: 'relay', names: this.names, pub });
      });
    }
  }

  sendData(remote, text, bytes) {
    const pair = this.pairs.get(remote);
    this.enqueue(pair, bytes, async seq => {
      const ciphertext = await this.subtle.encrypt({ name: 'AES-GCM', iv: frameNonce(this.seat, remote, seq),
        additionalData: frameAad(this.roomId, this.seat, remote, seq) }, pair.keys.send, utf8(text));
      return encodeBase64Url(new Uint8Array(ciphertext));
    });
  }

  enqueue(pair, bytes, makePayload) {
    if (this.closed) throw new Error('The relay connection is closed.');
    if (pair.sendSeq >= MAX_SEQ) throw new Error('Too many messages for one table.');
    const seq = ++pair.sendSeq;
    pair.sendChain = pair.sendChain.then(async () => {
      if (this.closed) return;
      const payload = await makePayload(seq);
      if (!this.closed) this.uploader.enqueue({ to: pair.remote, seq, payload, bytes });
    }).catch(error => this.fail(error));
  }

  // A `relay` event from the table stream; `from` was set by the server.
  receive({ from, seq, payload }) {
    if (this.closed) return;
    const pair = this.pairs.get(from);
    if (!pair || !Number.isSafeInteger(seq) || seq < 1 || typeof payload !== 'string' || payload.length > MAX_PAYLOAD_CHARS) {
      this.fail(new Error('The table server forwarded an invalid message.'));
      return;
    }
    if (seq <= pair.receiveSeq) return; // a replay after a reconnect or retry
    if (seq !== pair.receiveSeq + 1) {
      this.fail(new Error(`Messages from ${this.names[from]} were lost or reordered by the table server. The table has ended.`));
      return;
    }
    pair.receiveSeq = seq;
    pair.receiveChain = pair.receiveChain.then(() => this.process(pair, seq, payload)).catch(error => this.fail(error));
    if (this.getAck() - this.uploader.ackSent >= ACK_EVERY) this.uploader.requestAck();
  }

  async process(pair, seq, payload) {
    if (this.closed) return;
    const { privateKey, pub, authKey } = await this.setup;
    if (seq === 1) {
      const peerPub = await verifyKeyFrame(this.subtle, authKey, payload,
        { from: pair.remote, to: this.seat, room_id: this.roomId, names: this.names });
      pair.keys = await deriveDirectionKeys(this.subtle, { privateKey, secret: this.secret, roomId: this.roomId,
        seat: this.seat, remote: pair.remote, ownPub: pub, peerPub });
      if (!this.closed) pair.channel.open();
      return;
    }
    let text;
    try {
      const ciphertext = decodeBase64Url(payload);
      const plaintext = await this.subtle.decrypt({ name: 'AES-GCM', iv: frameNonce(pair.remote, this.seat, seq),
        additionalData: frameAad(this.roomId, pair.remote, this.seat, seq) }, pair.keys.receive, ciphertext);
      text = checkText(decoder.decode(plaintext));
    } catch {
      throw new Error(`A message from ${this.names[pair.remote]} failed its integrity check. The table has ended.`);
    }
    if (!this.closed) pair.channel.deliver(text);
  }

  fail(error) {
    if (this.closed) return;
    this.closed = true;
    this.uploader.close();
    this.onFatal(error);
    for (const pair of this.pairs.values()) pair.channel.close();
  }

  close() {
    if (this.closed) return;
    this.closed = true;
    this.uploader.close();
    for (const pair of this.pairs.values()) pair.channel.close(false);
  }
}
