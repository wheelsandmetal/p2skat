export const MAX_MESSAGE_BYTES = 64 * 1024;
const MAX_QUEUED_MESSAGES = 256;
const MAX_QUEUED_BYTES = 2 * 1024 * 1024;
const encoder = new TextEncoder();

export function checkText(text, limit = MAX_MESSAGE_BYTES) {
  if (typeof text !== 'string' || encoder.encode(text).length > limit) {
    throw new Error('Connection message is invalid or too large.');
  }
  return text;
}

export function validateNames(names) {
  if (!Array.isArray(names) || names.length !== 3 || names.some(name =>
    typeof name !== 'string' || !name.trim() || [...name].length > 40)) {
    throw new Error('Enter a name of 1–40 characters for each player.');
  }
  return names;
}

export function parseSignal(text, localSeat, remoteSeat, names) {
  const signal = JSON.parse(checkText(text));
  if (!signal || signal.version !== 1 || signal.from !== remoteSeat || signal.to !== localSeat ||
      JSON.stringify(signal.names) !== JSON.stringify(names) ||
      signal.type !== (remoteSeat < localSeat ? 'offer' : 'answer') ||
      typeof signal.sdp !== 'string' || !signal.sdp.startsWith('v=0\r\n')) {
    throw new Error('These connection details do not match this seat, player names, or pairing.');
  }
  return { type: signal.type, sdp: signal.sdp };
}

// Retain messages even when Wasm is loading or waiting for a player choice.
export class MessageQueue {
  constructor() {
    this.pending = [];
    this.bytes = 0;
    this.deliver = null;
  }

  push(seat, json) {
    checkText(json);
    if (this.deliver) {
      this.deliver(seat, json);
      return;
    }
    const bytes = encoder.encode(json).length;
    if (this.pending.length >= MAX_QUEUED_MESSAGES || this.bytes + bytes > MAX_QUEUED_BYTES) {
      throw new Error('Too many messages arrived before the game was ready.');
    }
    this.pending.push({ seat, json });
    this.bytes += bytes;
  }

  start(deliver) {
    this.deliver = deliver;
    for (const message of this.pending.splice(0)) deliver(message.seat, message.json);
    this.bytes = 0;
  }

  clear() {
    this.pending = [];
    this.bytes = 0;
    this.deliver = null;
  }
}

export class MeshReadiness {
  constructor(seat, names) {
    if (!Number.isInteger(seat) || seat < 0 || seat > 2) throw new Error('Invalid seat.');
    this.seat = seat;
    this.names = validateNames(names);
    this.hellos = new Set();
    this.ready = new Set();
  }

  receive(peer, message) {
    if (!Number.isInteger(peer) || peer < 0 || peer > 2 || peer === this.seat ||
        !message || message.version !== 1 || message.seat !== peer) {
      throw new Error('A connection sent an invalid seat or protocol version.');
    }
    if (message.type === 'hello') {
      if (this.hellos.has(peer) || JSON.stringify(message.names) !== JSON.stringify(this.names)) {
        throw new Error('Players disagree about the table names or a connection was repeated.');
      }
      this.hellos.add(peer);
    } else {
      if (!this.hellos.has(peer)) throw new Error('A peer sent messages before identifying itself.');
      if (message.type === 'ready') this.ready.add(peer);
      else if (message.type === 'game') {
        if (!this.ready.has(peer)) throw new Error('A peer started before the table was ready.');
        checkText(message.body);
      } else if (message.type !== 'bye') throw new Error('Unknown connection message.');
    }
    return this.hellos.size === 2 && this.ready.size === 2;
  }
}

export function iceConfiguration({ stun, turn, username, credential, relayOnly }) {
  const iceServers = [];
  if (stun.trim()) {
    if (!/^stuns?:[^\s]+$/.test(stun.trim())) throw new Error('Enter a valid stun: or stuns: URL.');
    iceServers.push({ urls: stun.trim() });
  }
  if (turn.trim()) {
    if (!/^turns?:[^\s]+$/.test(turn.trim()) || !username || !credential) {
      throw new Error('TURN needs a turn: or turns: URL and temporary username and password.');
    }
    iceServers.push({ urls: turn.trim(), username, credential });
  }
  if (relayOnly && !turn.trim()) throw new Error('Relay-only mode requires a TURN server.');
  return { iceServers, iceTransportPolicy: relayOnly ? 'relay' : 'all' };
}

function waitForIce(connection) {
  if (connection.iceGatheringState === 'complete') return Promise.resolve();
  return new Promise((resolve, reject) => {
    const finish = error => {
      clearTimeout(timer);
      connection.removeEventListener('icegatheringstatechange', changed);
      connection.removeEventListener('connectionstatechange', closed);
      error ? reject(error) : resolve();
    };
    const changed = () => { if (connection.iceGatheringState === 'complete') finish(); };
    const closed = () => { if (connection.connectionState === 'closed') finish(new Error('Connection closed.')); };
    const timer = setTimeout(() => finish(new Error('Network discovery timed out. Check your STUN/TURN settings.')), 45000);
    connection.addEventListener('icegatheringstatechange', changed);
    connection.addEventListener('connectionstatechange', closed);
  });
}

// Same-browser tabs need no network discovery or signaling exchange.
export class LocalDataChannel {
  constructor(room, seat, remote) {
    if (!/^[\w-]{16,128}$/.test(room) || seat === remote ||
        ![seat, remote].every(value => Number.isInteger(value) && value >= 0 && value < 3)) {
      throw new Error('Invalid local table pairing.');
    }
    this.label = 'p2skat';
    this.ordered = true;
    this.maxRetransmits = this.maxPacketLifeTime = null;
    this.bufferedAmount = 0;
    this.readyState = 'connecting';
    this.seat = seat;
    this.remote = remote;
    this.channel = new BroadcastChannel(`p2skat:${room}:${Math.min(seat, remote)}:${Math.max(seat, remote)}`);
    this.channel.onmessage = ({ data }) => {
      if (this.readyState === 'closed') return;
      try {
        if (!data || data.seat !== remote || data.to !== seat) throw new Error('Invalid local sender.');
        if (data.type === 'connect') {
          this.post('connected');
          this.open();
        } else if (data.type === 'connected') this.open();
        else if (data.type === 'data' && this.readyState === 'open') this.onmessage?.({ data: checkText(data.body) });
        else if (data.type === 'closed') this.close(false);
        else throw new Error('Invalid local connection message.');
      } catch (error) { this.onerror?.(error); }
    };
    this.channel.onmessageerror = () => this.onerror?.(new Error('Unreadable local connection message.'));
    this.post('connect');
  }

  post(type, body) {
    this.channel.postMessage({ type, seat: this.seat, to: this.remote, body });
  }

  open() {
    if (this.readyState !== 'connecting') return;
    this.readyState = 'open';
    this.onopen?.();
  }

  send(text) {
    if (this.readyState !== 'open') throw new Error('Local player connection is not open.');
    this.post('data', checkText(text));
  }

  close(notify = true) {
    if (this.readyState === 'closed') return;
    this.readyState = 'closed';
    if (notify) this.post('closed');
    this.channel.close();
    this.onclose?.();
  }
}

export class PeerMesh {
  constructor({ seat, names, configuration, localRoom, localPeers = [], relay = null, relayPeers = [], onSignal, onStatus, onMessage, onReady, onFailure }) {
    this.seat = seat;
    this.names = validateNames(names);
    this.readiness = new MeshReadiness(seat, names);
    this.onSignal = onSignal;
    this.onStatus = onStatus;
    this.onMessage = onMessage;
    this.onReady = onReady;
    this.onFailure = onFailure;
    this.peers = new Map();
    this.closed = false;
    this.announced = false;
    this.started = false;
    for (let remote = 0; remote < 3; remote++) {
      if (remote === seat) continue;
      if (localPeers.includes(remote)) {
        const channel = new LocalDataChannel(localRoom, seat, remote);
        this.peers.set(remote, { connection: { close: () => channel.close() }, local: true });
        this.attach(remote, channel);
        continue;
      }
      if (relayPeers.includes(remote)) {
        // End-to-end encrypted frames through the table server (web/relay.mjs).
        const channel = relay.channel(remote);
        this.peers.set(remote, { connection: { close: () => channel.close() }, relay: true });
        this.attach(remote, channel);
        continue;
      }
      const connection = new RTCPeerConnection(configuration);
      const peer = { connection, channel: null, applying: false };
      this.peers.set(remote, peer);
      connection.onconnectionstatechange = () => {
        if (['failed', 'disconnected', 'closed'].includes(connection.connectionState) && !this.closed) {
          this.fail(new Error(`${names[remote]} disconnected. The table has ended.`));
        }
      };
      connection.ondatachannel = event => {
        if (seat < remote || peer.channel) {
          event.channel.close();
          this.fail(new Error('An unexpected data channel was opened.'));
          return;
        }
        this.attach(remote, event.channel);
      };
      if (seat < remote) this.attach(remote, connection.createDataChannel('p2skat', { ordered: true }));
    }
  }

  async prepare() {
    await Promise.all([...this.peers].map(async ([remote, peer]) => {
      if (!peer.local && !peer.relay && this.seat < remote) {
        await peer.connection.setLocalDescription(await peer.connection.createOffer());
        await this.publishSignal(remote);
      }
    }));
  }

  async apply(remote, text) {
    const peer = this.peers.get(remote);
    const signal = parseSignal(text, this.seat, remote, this.names);
    if (this.closed || peer.applying || peer.connection.remoteDescription) {
      throw new Error('This pairing already has connection details. Leave and prepare a new table to retry.');
    }
    peer.applying = true;
    try {
      await peer.connection.setRemoteDescription(signal);
      peer.timer = setTimeout(() => {
        if (peer.channel?.readyState !== 'open') {
          this.fail(new Error(`Could not connect to ${this.names[remote]}. Try a working TURN server; this network may block direct connections.`));
        }
      }, 60000);
      if (signal.type === 'offer') {
        await peer.connection.setLocalDescription(await peer.connection.createAnswer());
        await this.publishSignal(remote);
      }
    } finally {
      peer.applying = false;
    }
  }

  async publishSignal(remote) {
    const connection = this.peers.get(remote).connection;
    await waitForIce(connection);
    if (this.closed) return;
    const { type, sdp } = connection.localDescription;
    const text = JSON.stringify({ version: 1, from: this.seat, to: remote, names: this.names, type, sdp });
    checkText(text);
    this.onSignal(remote, text);
    this.onStatus(remote, `Copy this ${type} to ${this.names[remote]}.`);
  }

  attach(remote, channel) {
    if (channel.label !== 'p2skat' || !channel.ordered || channel.maxRetransmits !== null || channel.maxPacketLifeTime !== null) {
      channel.close();
      this.fail(new Error('The game requires a reliable, ordered data channel.'));
      return;
    }
    this.peers.get(remote).channel = channel;
    let sentHello = false;
    channel.onopen = () => {
      if (sentHello || this.closed) return;
      sentHello = true;
      clearTimeout(this.peers.get(remote).timer);
      try {
        this.sendPacket(remote, { type: 'hello', names: this.names });
        this.onStatus(remote, 'Connected · checking table readiness');
      } catch (error) { this.fail(error); }
    };
    channel.onmessage = event => {
      if (this.closed) return;
      try {
        const message = JSON.parse(checkText(event.data));
        const ready = this.readiness.receive(remote, message);
        if (message.type === 'bye') {
          throw new Error(`${this.names[remote]} left. The table has ended.`);
        }
        if (message.type === 'game') this.onMessage(remote, message.body);
        if (this.readiness.hellos.size === 2 && !this.announced) {
          this.announced = true;
          for (const other of this.peers.keys()) this.sendPacket(other, { type: 'ready' });
        }
        if (message.type === 'ready') this.onStatus(remote, 'Connected · all of their pairings ready');
        if (ready && this.announced && !this.started) {
          this.started = true;
          this.onReady();
        }
      } catch (error) { this.fail(error); }
    };
    channel.onerror = () => this.fail(new Error(`The connection to ${this.names[remote]} failed.`));
    channel.onclose = () => { if (!this.closed) this.fail(new Error(`${this.names[remote]} disconnected. The table has ended.`)); };
    if (channel.readyState === 'open') channel.onopen();
  }

  sendPacket(remote, packet) {
    const { connection, channel } = this.peers.get(remote);
    const text = JSON.stringify({ ...packet, version: 1, seat: this.seat });
    const negotiated = connection.sctp?.maxMessageSize;
    checkText(text, negotiated > 0 ? Math.min(MAX_MESSAGE_BYTES, negotiated) : MAX_MESSAGE_BYTES);
    if (this.closed || channel?.readyState !== 'open') throw new Error('A player connection is not open.');
    if (channel.bufferedAmount + encoder.encode(text).length > MAX_QUEUED_BYTES) throw new Error('The network is too far behind to continue safely.');
    channel.send(text);
  }

  send(remote, json) {
    if (!this.started || !this.peers.has(remote)) throw new Error('The game sent to an unavailable player.');
    checkText(json);
    this.sendPacket(remote, { type: 'game', body: json });
  }

  fail(error) {
    if (this.closed) return;
    this.close();
    this.onFailure(error);
  }

  close() {
    if (this.closed) return;
    for (const remote of this.peers.keys()) {
      try { this.sendPacket(remote, { type: 'bye' }); } catch { /* A closed peer cannot receive a leave notice. */ }
    }
    this.closed = true;
    for (const { connection, timer } of this.peers.values()) {
      clearTimeout(timer);
      connection.close();
    }
  }
}
