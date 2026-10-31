import init, { crypto_self_check, run_game, deliver_message, submit_action, cancel_game } from './pkg/p2skat.js';
import { MessageQueue } from './transport.mjs';

const ready = init();
const incoming = new MessageQueue();
let running = false;
let stopped = false;

function report(error) {
  postMessage({ type: 'error', message: error?.message || String(error) });
}

self.onmessage = async ({ data }) => {
  try {
    if (data.type === 'receive') {
      incoming.push(data.seat, data.json);
      return;
    }
    if (data.type === 'cancel') {
      stopped = true;
      incoming.clear();
      await ready;
      cancel_game();
      return;
    }
    await ready;
    if (stopped) return;
    if (data.type === 'start') {
      if (running) throw new Error('A game is already running.');
      running = true;
      const game = run_game(JSON.stringify(data.config),
        (seat, json) => postMessage({ type: 'send', seat, json }),
        json => {
          const event = JSON.parse(json);
          if (event.type === 'ready') incoming.start((seat, payload) => deliver_message(seat, payload));
          postMessage({ type: 'render', event });
        });
      await game;
      postMessage({ type: 'ended' });
    } else if (data.type === 'action') {
      submit_action(data.json);
    } else if (data.type === 'self_check') {
      if (running) throw new Error('Run the self-check before starting a game.');
      const start = performance.now();
      const result = crypto_self_check();
      postMessage({ type: 'self_check', result: typeof result === 'string' ? JSON.parse(result) : result, elapsedMs: performance.now() - start });
    } else throw new Error('Unknown worker request.');
  } catch (error) { report(error); }
};

ready.catch(report);
