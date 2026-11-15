import { PeerMesh, MessageQueue, iceConfiguration, validateNames } from './transport.mjs';
import { RoomClient, browserMarker } from './rooms.mjs';
import { RelaySession, newInviteSecret, parseInviteSecret, relayInviteFragment } from './relay.mjs';

const $ = id => document.getElementById(id);
const ui = { seat: 0, names: [], hand: [], scores: [0, 0, 0], turn: null, choice: null, selected: new Set(), finished: false };
let mesh = null;
let worker = null;
let room = null;
let roomMeshReady = null;
let relay = null;
let tableMode = null;
let inviteSecret = null;
const pendingSignals = [];
const pendingRelay = [];
const incoming = new MessageQueue();
const peerPanels = new Map();

function element(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function status(message, error = false) {
  $('connection-state').textContent = message;
  $('connection-state').parentElement.classList.toggle('error', error);
}

function note(message) {
  $('activity').prepend(element('li', '', message));
  while ($('activity').children.length > 8) $('activity').lastElementChild.remove();
}

function stop(message, error = false) {
  if (ui.finished) return;
  ui.finished = true;
  ui.choice = null;
  ui.turn = null;
  mesh?.close();
  relay?.close();
  room?.close().catch(() => {});
  incoming.clear();
  worker?.postMessage({ type: 'cancel' });
  worker?.terminate();
  worker = null;
  $('actions').replaceChildren();
  $('actions').removeAttribute('data-kind');
  $('prompt-heading').textContent = 'Table ended';
  $('game-status').textContent = message;
  $('turn').textContent = 'Table ended';
  $('leave').hidden = true;
  $('self-check').disabled = true;
  $('self-check-result').textContent = '';
  for (const panel of peerPanels.values()) panel.apply.disabled = true;
  renderHand();
  renderPlayers();
  status(message, error);
  const restart = element('button', 'quiet', 'Set up a new table');
  restart.id = 'restart';
  restart.addEventListener('click', () => { location.href = location.pathname + location.search; });
  $('connection-state').parentElement.append(restart);
}

function getWorker() {
  if (worker) return worker;
  worker = new Worker(new URL('./worker.js', import.meta.url), { type: 'module' });
  worker.onerror = event => {
    event.preventDefault();
    stop(`Could not load the browser game. Build the Wasm package first and serve web/ over HTTP(S). ${event.message || ''}`, true);
  };
  worker.onmessageerror = () => stop('The game worker sent an unreadable message.', true);
  worker.onmessage = ({ data }) => {
    if (ui.finished) return;
    try {
      if (data.type === 'send') mesh.send(data.seat, data.json);
      else if (data.type === 'render') renderEvent(data.event);
      else if (data.type === 'error') stop(data.message, true);
      else if (data.type === 'ended') stop('The game finished. Thanks for playing.');
      else if (data.type === 'self_check') {
        if (!data.result.ok) throw new Error('The crypto self-check failed.');
        $('self-check-result').textContent = `Passed · ${data.result.cards} cards · ${data.result.prime_bits}-bit prime · ${(data.elapsedMs / 1000).toFixed(2)} s. This device’s result; try your phone too.`;
        $('self-check').disabled = Boolean(mesh?.started);
        $('self-check').textContent = 'Run crypto self-check';
      }
    } catch (error) { stop(error.message, true); }
  };
  return worker;
}

$('self-check').addEventListener('click', () => {
  $('self-check').disabled = true;
  $('self-check').textContent = 'Checking…';
  $('self-check-result').textContent = 'Loading Wasm, then shuffling and verifying in a worker…';
  try { getWorker().postMessage({ type: 'self_check' }); }
  catch (error) { stop(error.message, true); }
});

$('leave').addEventListener('click', () => stop('You left the table. The other players have been notified.'));
const fullscreen = $('fullscreen');
fullscreen.hidden = !document.fullscreenEnabled;
fullscreen.addEventListener('click', async () => {
  try {
    if (document.fullscreenElement) await document.exitFullscreen();
    else await document.documentElement.requestFullscreen();
  } catch { status('Full screen is unavailable in this browser.', true); }
});
document.addEventListener('fullscreenchange', () => {
  const label = document.fullscreenElement ? 'Exit full screen' : 'Enter full screen';
  fullscreen.setAttribute('aria-label', label);
  fullscreen.title = label;
});
window.addEventListener('pagehide', () => { mesh?.close(); relay?.close(); room?.close().catch(() => {}); worker?.terminate(); });

// Tor Browser has no RTCPeerConnection; an onion page must not leak IPs through WebRTC either.
// Either forces relay mode, where the table server forwards end-to-end encrypted frames.
const relayReason = typeof window.RTCPeerConnection !== 'function'
  ? 'This browser has no WebRTC (Tor Browser turns it off), so the game goes through the table server, end-to-end encrypted. STUN and TURN are not used.'
  : /\.onion$/i.test(location.hostname)
    ? 'This page is served over Tor, so the game goes through the table server, end-to-end encrypted, and no direct connections reveal your IP address. STUN and TURN are not used.'
    : null;
const viaTor = relayReason !== null;
const shuffleText = viaTor ? 'Shuffling over Tor, this can take half a minute…' : 'Shuffling through the table server…';

function readInvite() {
  const params = new URLSearchParams(location.hash.slice(1));
  return { room: params.get('room'), secret: params.get('k'), mode: params.get('m') };
}

// Decided before anything touches WebRTC: networkConfiguration() needs RTCPeerConnection.
function intendedMode(invite = readInvite()) {
  if (relayReason || invite.mode === 'relay') return 'relay';
  if (invite.room) return 'webrtc';
  return $('relay-mode').checked ? 'relay' : 'webrtc';
}

function updateNetworkUi() {
  const invite = readInvite();
  const relayMode = intendedMode(invite) === 'relay';
  $('ice-settings').hidden = relayMode;
  $('relay-choice').hidden = Boolean(relayReason || invite.room);
  $('relay-note').hidden = !relayMode;
  $('relay-note').textContent = relayReason || (invite.room
    ? 'This table routes the game through the table server, end-to-end encrypted. STUN and TURN are not used.'
    : 'The table server forwards end-to-end encrypted game messages. No direct connections, so STUN and TURN are not used.');
  $('privacy').textContent = relayMode
    ? `Other players never learn your IP address${/\.onion$/i.test(location.hostname) ? ', and over Tor neither does the table server' : '; the table server does, unless you use Tor'}. Keep this tab open: reloading it ends the game.`
    : 'Direct peers can see your IP. No Tor anonymity. Some networks need a working TURN relay to connect.';
}
$('relay-mode').addEventListener('change', updateNetworkUi);

function capabilityProblem() {
  if (typeof WebAssembly === 'undefined') return 'This browser blocks WebAssembly, which the card crypto needs. In Tor Browser, set Security Level to Standard for this site.';
  if (!window.isSecureContext || !globalThis.crypto?.subtle) return 'Open this page over HTTPS, an .onion address, or localhost: the browser only offers its crypto there.';
  if (!window.Worker) return 'This browser cannot run the game in a background worker.';
  return null;
}

function networkConfiguration() {
  if (!window.RTCPeerConnection || !window.Worker || !window.isSecureContext) {
    throw new Error('Use a browser with WebRTC and Web Workers over HTTPS, or localhost for development.');
  }
  return iceConfiguration({ stun: $('stun').value, turn: $('turn-url').value,
    username: $('turn-username').value, credential: $('turn-credential').value, relayOnly: $('relay-only').checked });
}

function startMesh(configuration, localPeers = [], relayPeers = []) {
  mesh = new PeerMesh({ seat: ui.seat, names: ui.names, configuration, localPeers, localRoom: room?.room, relay, relayPeers,
    onSignal: (remote, text) => {
      if (room) {
        room.sendSignal(remote, text).catch(error => stop(error.message, true));
      } else {
        const panel = peerPanels.get(remote);
        panel.outgoing.value = text;
        panel.copy.disabled = false;
      }
    },
    onStatus: (remote, text) => {
      if (room) $('room-progress').textContent = relay ? 'Keys exchanged · waiting for everyone…' : 'Connecting…';
      else peerPanels.get(remote).detail.textContent = text;
    },
    onMessage: (seat, json) => incoming.push(seat, json),
    onFailure: error => stop(error.message, true),
    onReady: () => {
      $('connections').hidden = true;
      $('room-lobby').hidden = true;
      $('game').hidden = false;
      $('self-check').disabled = true;
      status(relay ? '3 players connected through the table server · end-to-end encrypted' : '3 players connected');
      if (relay) $('game-status').textContent = shuffleText;
      renderPlayers();
      const gameWorker = getWorker();
      gameWorker.postMessage({ type: 'start', config: { seat: ui.seat, names: ui.names } });
      incoming.start((seat, json) => gameWorker.postMessage({ type: 'receive', seat, json }));
    },
  });
  return mesh.prepare();
}

async function applyRoomSignal(event) {
  try {
    await roomMeshReady;
    if (!ui.finished) await mesh.apply(event.from, event.signal);
  } catch (error) { stop(error.message, true); }
}

function showRoomPlayers(players, configuration, marker) {
  $('room-players').replaceChildren(...[0, 1, 2].map(seat => {
    const player = players.find(player => player.seat === seat);
    const item = element('li', `room-player${player ? ' present' : ''}`);
    item.append(element('span', 'room-seat', `${seat + 1}`), element('span', '', player ? `${player.name}${seat === ui.seat ? ' (you)' : ''}` : 'Open seat'));
    return item;
  }));
  if (players.length < 3) {
    $('room-progress').textContent = `${players.length} / 3 at the table`;
    return;
  }
  const names = validateNames(players.map(player => player.name));
  if (mesh) {
    if (JSON.stringify(names) !== JSON.stringify(ui.names)) stop('The table’s player list changed unexpectedly.', true);
    return;
  }
  ui.names = names;
  if (tableMode === 'relay') {
    $('room-progress').textContent = 'Everyone’s here. Exchanging keys…';
    relay = new RelaySession({ roomId: room.room, seat: ui.seat, names, secret: inviteSecret,
      post: (body, signal) => room.sendFrames(body, signal), getAck: () => room.lastEventId,
      onFatal: error => stop(error.message, true) });
    roomMeshReady = startMesh(null, [], [0, 1, 2].filter(seat => seat !== ui.seat));
    roomMeshReady.catch(error => stop(error.message, true));
    relay.start();
    for (const event of pendingRelay.splice(0)) relay.receive(event);
    return;
  }
  $('room-progress').textContent = 'Everyone’s here. Connecting…';
  const localPeers = typeof BroadcastChannel === 'function' && configuration.iceTransportPolicy !== 'relay'
    ? players.filter(player => player.seat !== ui.seat && player.browser === marker).map(player => player.seat) : [];
  roomMeshReady = startMesh(configuration, localPeers);
  roomMeshReady.catch(error => stop(error.message, true));
  for (const signal of pendingSignals.splice(0)) void applyRoomSignal(signal);
}

let invitedRoom;
function updateInvite() {
  if (room || mesh) return;
  invitedRoom = readInvite().room;
  updateNetworkUi();
  $('room-heading').textContent = invitedRoom ? 'Take your seat.' : 'Let’s play.';
  $('room-intro').textContent = invitedRoom
    ? 'Your friends are waiting.'
    : 'Create a table. Invite two friends.';
  $('create-table').hidden = Boolean(invitedRoom);
  $('join-table').hidden = !invitedRoom;
}
updateInvite();
window.addEventListener('hashchange', updateInvite);
const unsupported = capabilityProblem();
if (unsupported) {
  status(unsupported, true);
  for (const id of ['create-table', 'join-table', 'prepare', 'self-check']) $(id).disabled = true;
}

async function enterRoom() {
  if (!$('room-form').reportValidity()) return;
  $('create-table').disabled = $('join-table').disabled = true;
  try {
    const unsupportedNow = capabilityProblem();
    if (unsupportedNow) throw new Error(unsupportedNow);
    const inviteParams = readInvite();
    if (inviteParams.mode !== null && inviteParams.mode !== 'relay') throw new Error('This invite link is invalid.');
    const mode = intendedMode(inviteParams);
    let configuration = null;
    if (mode === 'relay' && invitedRoom && inviteParams.mode !== 'relay' && !inviteParams.secret) {
      throw new Error('This invite is for a table with direct WebRTC connections, which this browser cannot make, or the link is incomplete. Ask for a table that routes the game through the table server.');
    }
    if (mode === 'relay') inviteSecret = invitedRoom ? parseInviteSecret(inviteParams.secret) : newInviteSecret();
    else configuration = networkConfiguration();
    const name = $('player-name').value.trim();
    if (!name) throw new Error('Enter your name to join the table.');
    // The same-browser BroadcastChannel shortcut is WebRTC-mode only.
    const marker = mode === 'webrtc' && typeof BroadcastChannel === 'function' && configuration.iceTransportPolicy !== 'relay'
      ? browserMarker() : crypto.randomUUID();
    try {
      room = await RoomClient.enter({ room: invitedRoom, name, browser: marker, mode });
    } catch (error) {
      if (error.code !== 'wrong_mode') throw error;
      throw new Error(mode === 'relay'
        ? 'This table uses direct WebRTC connections, which this browser cannot or should not make. Ask for a table that routes the game through the table server.'
        : 'This table routes the game through the table server, but the invite link lacks that setting. Ask for the complete invite link again.');
    }
    tableMode = mode;
    ui.seat = room.seat;
    const invite = new URL(location.href);
    invite.hash = mode === 'relay' ? relayInviteFragment(room.room, inviteSecret) : `room=${room.room}`;
    $('invite-link').value = invite.href;
    $('open-player-tab').href = invite.href;
    $('setup').hidden = true;
    $('room-lobby').hidden = false;
    $('leave').hidden = false;
    $('relay-warning').hidden = mode !== 'relay';
    status(`You’re at the table · seat ${ui.seat + 1}.`);
    room.listen(event => {
      if (event.type === 'players') showRoomPlayers(event.players, configuration, marker);
      else if (event.type === 'closed') stop(event.message);
      else if (event.type === 'relay') {
        if (relay) relay.receive(event);
        else {
          if (pendingRelay.length >= 8) throw new Error('Too many messages arrived before the table was ready.');
          pendingRelay.push(event);
        }
      } else if (mesh) void applyRoomSignal(event);
      else {
        if (pendingSignals.length >= 4) throw new Error('Too many connection details arrived before the table was ready.');
        pendingSignals.push(event);
      }
    }, error => {
      // A relay game lives on this stream; a WebRTC game no longer needs it once started.
      if (tableMode === 'relay' || !mesh?.started) stop(error.message, true);
    });
    showRoomPlayers(room.players, configuration, marker);
  } catch (error) {
    if (room || mesh) stop(error.message, true);
    else {
      status(error.message, true);
      $('create-table').disabled = $('join-table').disabled = false;
    }
  }
}

$('room-form').addEventListener('submit', event => { event.preventDefault(); void enterRoom(); });
$('join-table').addEventListener('click', () => void enterRoom());
$('copy-invite').addEventListener('click', async () => {
  try {
    await navigator.clipboard.writeText($('invite-link').value);
    $('copy-invite').textContent = 'Invite copied';
  } catch {
    $('invite-link').focus();
    $('invite-link').select();
    $('room-progress').textContent = 'Copy the highlighted invite link and send it to your friends.';
  }
});

function buildPeerPanel(remote) {
  const panel = element('section', 'peer-panel');
  panel.dataset.peer = remote;
  const title = element('h2', '', `Seat ${ui.seat + 1} ↔ Seat ${remote + 1} · ${ui.names[remote]}`);
  const detail = element('p', 'peer-status', ui.seat < remote ? 'Gathering network candidates for your offer…' : `Waiting for an offer from ${ui.names[remote]}.`);
  const outLabel = element('label', '', `Your details for ${ui.names[remote]}`);
  const outgoing = element('textarea', 'signal-out');
  outgoing.readOnly = true;
  outgoing.rows = 3;
  outgoing.spellcheck = false;
  outgoing.placeholder = ui.seat < remote ? 'Preparing offer…' : 'Your answer appears here after applying their offer.';
  outLabel.append(outgoing);
  const copy = element('button', 'signal-copy', 'Copy your details');
  copy.disabled = true;
  copy.addEventListener('click', async () => {
    try {
      await navigator.clipboard.writeText(outgoing.value);
      copy.textContent = 'Copied';
    } catch {
      outgoing.focus();
      outgoing.select();
      detail.textContent = 'Select and copy the highlighted details using your browser.';
    }
  });
  const inLabel = element('label', '', `${ui.names[remote]}’s ${ui.seat < remote ? 'answer' : 'offer'}`);
  const input = element('textarea', 'signal-in');
  input.rows = 3;
  input.spellcheck = false;
  input.maxLength = 65536;
  input.placeholder = 'Paste the complete connection details here';
  inLabel.append(input);
  const apply = element('button', 'signal-apply', 'Apply connection details');
  apply.addEventListener('click', async () => {
    apply.disabled = true;
    try {
      await mesh.apply(remote, input.value);
      input.readOnly = true;
      detail.textContent = ui.seat < remote ? 'Answer accepted · connecting…' : 'Offer accepted · copy your answer back.';
    } catch (error) {
      detail.textContent = error.message;
      apply.disabled = false;
    }
  });
  panel.append(title, detail, outLabel, copy, inLabel, apply);
  peerPanels.set(remote, { panel, detail, outgoing, copy, apply });
  return panel;
}

$('setup-form').addEventListener('submit', async event => {
  event.preventDefault();
  try {
    const configuration = networkConfiguration();
    ui.seat = Number($('seat').value);
    ui.names = validateNames([0, 1, 2].map(seat => $(`name-${seat}`).value.trim()));
    $('peer-connections').replaceChildren();
    for (let remote = 0; remote < 3; remote++) if (remote !== ui.seat) $('peer-connections').append(buildPeerPanel(remote));
    $('setup').hidden = true;
    $('connections').hidden = false;
    $('leave').hidden = false;
    status(`You are ${ui.names[ui.seat]} · seat ${ui.seat + 1}. Connect both pairings below.`);
    await startMesh(configuration);
  } catch (error) {
    if (mesh) stop(error.message, true);
    else status(error.message, true);
  }
});

const suits = ['diamonds', 'hearts', 'spades', 'clubs'];
const symbols = ['♦', '♥', '♠', '♣'];
const ranks = ['7', '8', '9', '10', 'J', 'Q', 'K', 'A'];
const rankNames = ['Seven', 'Eight', 'Nine', 'Ten', 'Jack', 'Queen', 'King', 'Ace'];

function cardNode(id, button = false) {
  if (!Number.isInteger(id) || id < 1 || id > 32) throw new Error('The game supplied an invalid card.');
  const suit = Math.floor((id - 1) / 8);
  const rank = (id - 1) % 8;
  const node = element(button ? 'button' : 'div', `card${suit < 2 ? ' red' : ''}`);
  node.dataset.card = id;
  node.setAttribute('aria-label', `${rankNames[rank]} of ${suits[suit]}`);
  if (!button) node.setAttribute('role', 'img');
  for (const [className, text] of [['rank', ranks[rank]], ['suit', symbols[suit]], ['rank-end', ranks[rank]]]) {
    const part = element('span', className, text);
    part.setAttribute('aria-hidden', 'true');
    node.append(part);
  }
  return node;
}

function renderPlayers() {
  $('players').replaceChildren(...ui.names.map((name, seat) => {
    const node = element('div', `player${ui.turn === seat ? ' active' : ''}`);
    node.dataset.seat = seat;
    const avatar = element('span', 'player-avatar', `${seat + 1}`);
    avatar.setAttribute('aria-hidden', 'true');
    const detail = element('div');
    detail.append(element('div', 'player-name', `${name}${seat === ui.seat ? ' (you)' : ''}`), element('div', 'player-score', `${ui.scores[seat] > 0 ? '+' : ''}${ui.scores[seat]} points`));
    node.append(avatar, detail);
    return node;
  }));
}

function renderHand() {
  $('hand').replaceChildren(...ui.hand.map(id => {
    const card = cardNode(id, true);
    const choice = ui.choice;
    const discard = choice?.kind === 'discard';
    const legal = choice?.kind === 'card' && choice.legal.includes(id);
    card.disabled = ui.finished || !(discard || legal);
    if (legal) card.classList.add('legal');
    if (discard) {
      card.setAttribute('aria-pressed', String(ui.selected.has(id)));
      card.classList.toggle('selected', ui.selected.has(id));
      card.addEventListener('click', () => {
        if (ui.selected.has(id)) ui.selected.delete(id);
        else if (ui.selected.size < 2) ui.selected.add(id);
        renderHand();
        $('discard-count').textContent = `${ui.selected.size} / 2 selected`;
        $('discard-submit').disabled = ui.selected.size !== 2;
        $('hand').querySelector(`[data-card="${id}"]`).focus();
      });
    } else if (legal) card.addEventListener('click', () => submit({ card: id }));
    return card;
  }));
  $('hand-hint').textContent = ui.choice?.kind === 'discard' ? 'Choose two to discard' : ui.choice?.kind === 'card' ? 'Gold marks playable cards' : '';
}

function renderTrick(cards) {
  $('trick').replaceChildren(...cards.map(({ seat, card }) => {
    const play = element('div', 'trick-play');
    play.append(element('span', '', ui.names[seat]), cardNode(card));
    return play;
  }));
  if (!cards.length) {
    const empty = element('p', 'empty-trick', '♠');
    empty.setAttribute('role', 'img');
    empty.setAttribute('aria-label', 'No cards played');
    $('trick').append(empty);
  }
}

function renderContract(contract) {
  $('contract').textContent = `${contract.game_type === 'suit' ? contract.trump : contract.game_type}${contract.hand ? ' · hand' : ''}`;
}

function submit(action) {
  if (!ui.choice || ui.finished) return;
  const id = ui.choice.id;
  ui.choice = null;
  ui.selected.clear();
  $('actions').replaceChildren();
  $('actions').removeAttribute('data-kind');
  $('prompt-heading').textContent = 'At the table';
  $('game-status').textContent = 'Waiting for the others…';
  renderHand();
  worker.postMessage({ type: 'action', json: JSON.stringify({ ...action, id }) });
}

function actionButton(label, action, value, primary = false) {
  const button = element('button', primary ? 'primary' : '', label);
  button.dataset.action = action;
  button.addEventListener('click', () => submit(value));
  return button;
}

function renderChoice(choice) {
  ui.choice = choice;
  ui.selected.clear();
  if (choice.hand) ui.hand = choice.hand;
  if (choice.trick) renderTrick(choice.trick);
  if (choice.contract) renderContract(choice.contract);
  if (choice.trick_num) $('trick-num').textContent = `Trick ${choice.trick_num} / 10`;
  ui.turn = choice.kind === 'continue' ? null : ui.seat;
  $('turn').textContent = choice.kind === 'continue' ? 'Round complete' : 'Your turn';
  renderPlayers();
  const actions = $('actions');
  actions.replaceChildren();
  actions.dataset.kind = choice.kind;
  const buttons = element('div', 'action-buttons');
  if (choice.kind === 'bid') {
    $('prompt-heading').textContent = 'Your bid';
    $('game-status').textContent = choice.is_responder ? 'Hold the bid or pass.' : 'Raise the bid or pass.';
    if (choice.is_responder) buttons.append(actionButton('Hold', 'hold', { action: 'hold' }, true));
    else if (choice.next_bid !== null) buttons.append(actionButton(`Bid ${choice.next_bid}`, 'bid', { action: 'bid' }, true));
    buttons.append(actionButton('Pass', 'pass', { action: 'pass' }));
  } else if (choice.kind === 'pickup') {
    $('prompt-heading').textContent = 'Skat or hand?';
    $('game-status').textContent = 'Take two cards, or keep your ten.';
    buttons.append(actionButton('Pick up skat', 'pickup', { pickup: true }, true), actionButton('Play hand', 'hand', { pickup: false }));
  } else if (choice.kind === 'discard') {
    $('prompt-heading').textContent = 'Discard two';
    $('game-status').textContent = 'Select two cards for the skat.';
    const count = element('p', 'selected-cards', '0 / 2 selected');
    count.id = 'discard-count';
    count.setAttribute('role', 'status');
    const button = element('button', 'primary', 'Discard selected');
    button.id = 'discard-submit';
    button.dataset.action = 'discard';
    button.disabled = true;
    button.addEventListener('click', () => submit({ cards: [...ui.selected] }));
    actions.append(count, button);
  } else if (choice.kind === 'contract') {
    $('prompt-heading').textContent = 'Name your game';
    $('game-status').textContent = `Bid ${choice.bid}${choice.is_hand ? ' · Hand' : ''}`;
    const form = element('form');
    const label = element('label', '', 'Game');
    const type = element('select');
    type.id = 'contract-type';
    const games = [['suit', 'Suit game'], ['grand', 'Grand · jacks are trumps']];
    // Null has a fixed value; the game core rejects it when the bid is higher.
    if (choice.null_allowed) games.push(['null', 'Null · take no tricks']);
    for (const [value, text] of games) {
      const option = element('option', '', text);
      option.value = value;
      type.append(option);
    }
    label.append(type);
    const trumpLabel = element('label', '', 'Trump suit');
    const trump = element('select');
    trump.id = 'contract-trump';
    for (const [index, value] of suits.entries()) {
      const option = element('option', '', `${symbols[index]} ${value[0].toUpperCase()}${value.slice(1)}`);
      option.value = value;
      trump.append(option);
    }
    trumpLabel.append(trump);
    type.addEventListener('change', () => { trumpLabel.hidden = type.value !== 'suit'; });
    const declare = element('button', 'primary', 'Declare');
    declare.type = 'submit';
    declare.dataset.action = 'declare';
    form.append(label, trumpLabel, declare);
    form.addEventListener('submit', event => {
      event.preventDefault();
      submit({ game_type: type.value, trump: trump.value });
    });
    actions.append(form);
  } else if (choice.kind === 'card') {
    $('prompt-heading').textContent = 'Play a card';
    $('game-status').textContent = 'Follow suit when you can.';
  } else if (choice.kind === 'continue') {
    $('prompt-heading').textContent = 'Another round?';
    $('game-status').textContent = 'Everyone agrees to deal again.';
    buttons.append(actionButton('Next round', 'continue', { continue: true }, true), actionButton('Finish game', 'finish', { continue: false }));
  } else throw new Error('Unknown game choice.');
  if (buttons.children.length) actions.append(buttons);
  renderHand();
  const target = choice.kind === 'card' || choice.kind === 'discard' ? $('hand').querySelector('button:enabled') : actions.querySelector('button:enabled, select');
  target?.focus({ preventScroll: true });
}

function renderEvent(event) {
  if (event.type === 'ready') return;
  if (event.type === 'choice') renderChoice(event);
  else if (event.type === 'hand') { ui.hand = event.cards; renderHand(); }
  else if (event.type === 'turn') {
    ui.turn = event.seat;
    $('turn').textContent = event.seat === ui.seat ? 'Your turn' : `${ui.names[event.seat]}’s turn`;
    renderPlayers();
  } else if (event.type === 'trick') renderTrick(event.cards);
  else if (event.type === 'contract') renderContract(event.contract);
  else if (event.type === 'scores') { ui.scores = event.scores; renderPlayers(); }
  else if (event.type === 'trick_num') $('trick-num').textContent = `Trick ${event.number} / 10`;
  else if (event.type === 'round') {
    $('round').textContent = `Round ${event.number}`;
    $('contract').textContent = 'Dealing';
    $('trick-num').textContent = 'Trick — / 10';
    ui.hand = [];
    ui.turn = null;
    renderHand();
    renderPlayers();
    renderTrick([]);
    note(`Round ${event.number} · a fresh deck.`);
  } else if (event.type === 'status') {
    if (relay && /^Shuffling/.test(event.message)) event = { ...event, message: shuffleText };
    $('game-status').textContent = event.message;
    if (!ui.choice) $('prompt-heading').textContent = 'At the table';
    note(event.message);
  } else if (event.type === 'bid') {
    note(`${event.name} ${event.action === 'bid' ? `bids ${event.value}` : event.action === 'hold' ? 'holds' : 'passes'}.`);
  } else if (event.type === 'trick_result') note(`${event.winner} takes the trick · ${event.points} points.`);
  else if (event.type === 'round_result') {
    ui.scores = event.scores;
    ui.turn = null;
    $('turn').textContent = 'Round complete';
    renderPlayers();
    const result = `${event.declarer}: ${event.value >= 0 ? '+' : ''}${event.value} · ${event.points} card points.`;
    note(result);
    status(`Round verified. ${result}`);
  }
}
