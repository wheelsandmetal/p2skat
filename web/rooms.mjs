import { checkText } from './transport.mjs';

export function validatePlayers(players) {
  if (!Array.isArray(players) || players.length < 1 || players.length > 3 ||
      players.some(player => !player || !Number.isInteger(player.seat) || player.seat < 0 || player.seat > 2 ||
        typeof player.name !== 'string' || !player.name.trim() || player.name.length > 40 ||
        typeof player.browser !== 'string' || !player.browser || player.browser.length > 48) ||
      new Set(players.map(player => player.seat)).size !== players.length) {
    throw new Error('The table service sent an invalid player list.');
  }
  return [...players].sort((a, b) => a.seat - b.seat);
}

export function parseRoomEvent(text, seat) {
  const event = JSON.parse(checkText(text, 128 * 1024));
  if (event?.type === 'players') return { ...event, players: validatePlayers(event.players) };
  if (event?.type === 'signal' && Number.isInteger(event.from) && event.from >= 0 && event.from < 3 && event.from !== seat) {
    checkText(event.signal);
    return event;
  }
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
    throw new Error('Invite tables need the room server. Restart with python3 scripts/serve.py, then reload this page.');
  }
  let data;
  try { data = await response.json(); }
  catch { throw new Error('The table service is unavailable. Restart with python3 scripts/serve.py, then reload this page.'); }
  if (!response.ok) throw new Error(typeof data.error === 'string' ? data.error : 'The table request failed.');
  return data;
}

export class RoomClient {
  static async enter({ room, name, browser, fetcher = globalThis.fetch, EventSourceClass = globalThis.EventSource }) {
    if (room && !/^[a-zA-Z0-9_-]{16,128}$/.test(room)) throw new Error('This invite link is invalid.');
    const path = room ? `/api/rooms/${encodeURIComponent(room)}/join` : '/api/rooms';
    const data = await request(path, { name, browser }, null, fetcher);
    if (!data || !/^[a-zA-Z0-9_-]{16,128}$/.test(data.room) || !/^[a-zA-Z0-9_-]{16,128}$/.test(data.token) ||
        !Number.isInteger(data.seat) || data.seat < 0 || data.seat > 2) throw new Error('The table service returned invalid seat details.');
    data.players = validatePlayers(data.players);
    if (!data.players.some(player => player.seat === data.seat && player.name === name && player.browser === browser)) {
      throw new Error('The table service returned a seat belonging to another player.');
    }
    return new RoomClient(data, fetcher, EventSourceClass);
  }

  constructor(data, fetcher, EventSourceClass) {
    Object.assign(this, data);
    this.fetcher = fetcher;
    this.EventSourceClass = EventSourceClass;
    this.closed = false;
    this.source = null;
  }

  listen(onEvent, onError) {
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

  sendSignal(to, signal) {
    if (this.closed) return Promise.reject(new Error('This table has ended.'));
    return request(`/api/rooms/${encodeURIComponent(this.room)}/signal`, { to, signal }, this.token, this.fetcher);
  }

  disconnectEvents() {
    if (!this.source) return;
    this.source.onmessage = this.source.onerror = null;
    this.source.close();
  }

  async close() {
    if (this.closed) return;
    this.closed = true;
    this.disconnectEvents();
    await request(`/api/rooms/${encodeURIComponent(this.room)}/leave`, {}, this.token, this.fetcher);
  }
}
