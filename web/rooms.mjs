import { checkText } from './transport.mjs';

const ROOM_ID = /^[a-zA-Z0-9_-]{16,128}$/;
const MAX_PAYLOAD_CHARS = Math.ceil((64 * 1024 + 64) * 4 / 3);
const MODES = ['webrtc', 'relay'];
const SERVER_HINT = 'Start the table server (cargo run --release -p p2skat-room, or python3 scripts/serve.py), then reload this page.';
// Relay tables only: the server sends a named `ping` after 10 s of silence. Without any event for
// WATCHDOG_MS the stream is presumed dead and reopened by hand from the last event id.
export const WATCHDOG_MS = 25000;
const RECONNECT_MS = 2000;
const GIVE_UP_MS = 150000;
// Relay tables only: the server closes a table when a player sends no POST for 120 s (a frozen
// tab can keep its stream open). Without other uploads, an ack-only POST goes out this often.
export const KEEPALIVE_MS = 30000;
export const RESTARTED_MESSAGE = 'The table server restarted or closed the table.';

// Names are limited in code points, as the server counts them (not UTF-16 units).
export const nameLength = name => [...name].length;

export function validatePlayers(players) {
  if (!Array.isArray(players) || players.length < 1 || players.length > 3 ||
      players.some(player => !player || !Number.isInteger(player.seat) || player.seat < 0 || player.seat > 2 ||
        typeof player.name !== 'string' || !player.name.trim() || nameLength(player.name) > 40 ||
        typeof player.browser !== 'string' || !player.browser || player.browser.length > 48) ||
      new Set(players.map(player => player.seat)).size !== players.length) {
    throw new Error('The table service sent an invalid player list.');
  }
  return [...players].sort((a, b) => a.seat - b.seat);
}

// `mode` (when known) rejects events that belong to the other kind of table.
export function parseRoomEvent(text, seat, mode) {
  const event = JSON.parse(checkText(text, 128 * 1024));
  const peer = Number.isInteger(event?.from) && event.from >= 0 && event.from < 3 && event.from !== seat;
  if (event?.type === 'players') return { ...event, players: validatePlayers(event.players) };
  if (event?.type === 'signal' && mode !== 'relay' && peer) {
    checkText(event.signal);
    return event;
  }
  if (event?.type === 'relay' && mode !== 'webrtc' && peer && Number.isSafeInteger(event.seq) && event.seq >= 1 &&
      typeof event.payload === 'string' && event.payload.length <= MAX_PAYLOAD_CHARS && /^[A-Za-z0-9_-]+$/.test(event.payload)) {
    return { type: 'relay', from: event.from, seq: event.seq, payload: event.payload };
  }
  if (event?.type === 'ping') return { type: 'ping' };
  if (event?.type === 'closed' && typeof event.message === 'string' && event.message.length <= 400) return event;
  throw new Error('The table service sent an invalid event.');
}

export function browserMarker(storage, crypto = globalThis.crypto) {
  let marker;
  try {
    if (storage === undefined) storage = globalThis.localStorage;
    marker = storage.getItem('p2skat-browser');
  } catch { /* Private browser settings may disable storage. */ }
  if (typeof marker === 'string' && /^[a-zA-Z0-9-]{16,48}$/.test(marker)) return marker;
  marker = crypto.randomUUID();
  try { storage.setItem('p2skat-browser', marker); } catch { /* Separate-tab optimization is optional. */ }
  return marker;
}

async function request(path, body, token, fetcher) {
  const response = await fetcher(path, {
    method: 'POST', headers: { 'Content-Type': 'application/json', ...(token ? { Authorization: `Bearer ${token}` } : {}) },
    body: JSON.stringify(body), keepalive: path.endsWith('/leave'),
  });
  if (response.status === 404 && !(response.headers.get('Content-Type') || '').includes('application/json')) {
    throw new Error(`Invite tables need the table server. ${SERVER_HINT}`);
  }
  let data;
  try { data = await response.json(); }
  catch { throw new Error(`The table service is unavailable. ${SERVER_HINT}`); }
  if (!response.ok) {
    const error = new Error(typeof data?.error === 'string' ? data.error : 'The table request failed.');
    error.code = data?.code;
    throw error;
  }
  return data;
}

export class RoomClient {
  // `mode` is what this client intends. The server's answer is checked, not trusted: a relay
  // player never falls back to WebRTC (which would reveal its IP address to the other players).
  static async enter({ room, name, browser, mode, fetcher = globalThis.fetch, EventSourceClass = globalThis.EventSource, timers, keepaliveMs }) {
    if (room && !ROOM_ID.test(room)) throw new Error('This invite link is invalid.');
    if (mode !== undefined && !MODES.includes(mode)) throw new Error('Unknown table mode.');
    const path = room ? `/api/rooms/${encodeURIComponent(room)}/join` : '/api/rooms';
    const data = await request(path, mode ? { name, browser, mode } : { name, browser }, null, fetcher);
    if (!data || !ROOM_ID.test(data.room) || !ROOM_ID.test(data.token) || (room && data.room !== room) ||
        !Number.isInteger(data.seat) || data.seat < 0 || data.seat > 2) throw new Error('The table service returned invalid seat details.');
    if (mode && data.mode !== mode) {
      throw new Error(mode === 'relay'
        ? 'The table server did not set up a relayed table. Refusing to connect directly to other players.'
        : 'The table server set up a different kind of table than requested.');
    }
    data.players = validatePlayers(data.players);
    if (!data.players.some(player => player.seat === data.seat && player.name === name && player.browser === browser)) {
      throw new Error('The table service returned a seat belonging to another player.');
    }
    return new RoomClient(data, fetcher, EventSourceClass, timers, keepaliveMs);
  }

  constructor(data, fetcher, EventSourceClass, timers = globalThis, keepaliveMs = KEEPALIVE_MS) {
    Object.assign(this, data);
    this.fetcher = fetcher;
    this.EventSourceClass = EventSourceClass;
    this.timers = timers;
    this.keepaliveMs = keepaliveMs;
    this.now = () => Date.now();
    this.lastPost = this.now(); // joining was a POST
    this.probing = false;
    this.closed = false;
    this.source = null;
    this.lastEventId = 0;
    this.lastActivity = 0;
    this.lostSince = null;
    this.watchdog = null;
    this.reconnectTimer = null;
  }

  eventsUrl() {
    const after = this.lastEventId ? `&after=${this.lastEventId}` : '';
    return `/api/rooms/${encodeURIComponent(this.room)}/events?token=${encodeURIComponent(this.token)}${after}`;
  }

  listen(onEvent, onError) {
    if (this.mode === 'relay') {
      this.onEvent = onEvent;
      this.onError = onError;
      this.connect();
      this.watchdog = this.timers.setInterval(() => this.checkStream(), 1000);
      return;
    }
    this.source = new this.EventSourceClass(`/api/rooms/${encodeURIComponent(this.room)}/events?token=${encodeURIComponent(this.token)}`);
    this.source.onmessage = ({ data }) => {
      if (this.closed) return;
      try { onEvent(parseRoomEvent(data, this.seat)); }
      catch (error) { this.disconnectEvents(); onError(error); }
    };
    this.source.onerror = () => {
      this.disconnectEvents();
      if (!this.closed) onError(new Error('Connection setup lost contact with the table service. Reload the invite to try again.'));
    };
  }

  // Relay tables: a stream that fails is reopened from the last event id; the server keeps
  // unacknowledged events, and RelaySession drops anything replayed twice.
  connect() {
    this.closeSource();
    if (this.closed) return;
    const source = this.source = new this.EventSourceClass(this.eventsUrl());
    this.lastActivity = this.now();
    source.onopen = () => this.touch();
    source.addEventListener?.('ping', () => this.touch());
    source.onmessage = event => this.receive(event);
    source.onerror = () => {
      // CONNECTING means EventSource retries by itself with Last-Event-ID; CLOSED means it gave up.
      if (this.closed || source !== this.source || source.readyState !== 2) return;
      this.lostSince ??= this.now();
      // EventSource does not expose the status; a 404 or 401 means the table is gone for good.
      void this.probe();
      if (!this.reconnectTimer) {
        this.reconnectTimer = this.timers.setTimeout(() => { this.reconnectTimer = null; this.connect(); }, RECONNECT_MS);
      }
    };
  }

  touch() {
    this.lastActivity = this.now();
    this.lostSince = null;
  }

  receive({ data, lastEventId }) {
    if (this.closed) return;
    this.touch();
    try {
      const event = parseRoomEvent(data, this.seat, this.mode);
      if (event.type === 'ping') return;
      if (event.type !== 'players') {
        // Logged events carry consecutive ids; anything at or below the last one is a replay.
        const id = Number(lastEventId);
        if (!Number.isSafeInteger(id) || id < 1) throw new Error('The table service sent an event without an id.');
        if (id <= this.lastEventId) return;
        this.lastEventId = id;
      }
      this.onEvent(event);
    } catch (error) { this.fail(error); }
  }

  checkStream() {
    if (this.closed) return;
    const now = this.now();
    if (this.lostSince !== null && now - this.lostSince > GIVE_UP_MS) {
      this.fail(new Error('Lost contact with the table server. The table has ended.'));
      return;
    } else if (now - this.lastActivity > WATCHDOG_MS) {
      this.lostSince ??= this.lastActivity;
      this.connect();
    }
    if (now - this.lastPost >= this.keepaliveMs) void this.probe();
  }

  // An ack-only relay POST: the server's liveness keepalive, and a probe that tells a table the
  // server forgot (it restarted, or the closed table expired) from a network problem.
  async probe() {
    if (this.closed || this.probing) return;
    this.probing = true;
    try {
      const { status, data } = await this.sendFrames({ frames: [], ack: this.lastEventId });
      if (status === 401 || (status === 404 && data?.code === 'not_found')) this.fail(new Error(RESTARTED_MESSAGE));
    } catch { /* Network trouble: the stream watchdog and the uploader decide. */ }
    finally { this.probing = false; }
  }

  fail(error) {
    this.disconnectEvents();
    if (!this.closed) this.onError(error);
  }

  // Relay tables: one batched POST of encrypted frames (see RelayUploader). Returns the status and
  // JSON body without throwing for API errors; network errors and aborts reject.
  async sendFrames(body, signal) {
    if (this.closed) throw new Error('This table has ended.');
    const fetcher = this.fetcher; // native fetch throws when called as a method of another object
    this.lastPost = this.now();
    const response = await fetcher(`/api/rooms/${encodeURIComponent(this.room)}/relay`, {
      method: 'POST', headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${this.token}` },
      body: JSON.stringify(body), signal,
    });
    let data = null;
    try { data = await response.json(); } catch { /* A proxy error page; the uploader decides by status. */ }
    return { status: response.status, data };
  }

  sendSignal(to, signal) {
    if (this.closed) return Promise.reject(new Error('This table has ended.'));
    return request(`/api/rooms/${encodeURIComponent(this.room)}/signal`, { to, signal }, this.token, this.fetcher);
  }

  closeSource() {
    if (!this.source) return;
    this.source.onmessage = this.source.onerror = this.source.onopen = null;
    this.source.close();
  }

  disconnectEvents() {
    this.timers.clearInterval?.(this.watchdog);
    this.timers.clearTimeout?.(this.reconnectTimer);
    this.watchdog = this.reconnectTimer = null;
    this.closeSource();
  }

  async close() {
    if (this.closed) return;
    this.closed = true;
    this.disconnectEvents();
    await request(`/api/rooms/${encodeURIComponent(this.room)}/leave`, {}, this.token, this.fetcher);
  }
}
