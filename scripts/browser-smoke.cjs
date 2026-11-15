// Run against the built, served web/ directory with playwright-core on NODE_PATH.
//   node scripts/browser-smoke.cjs [--relay] [URL]
// --relay (or TEST_RELAY=1): isolated browser profiles without RTCPeerConnection, like Tor Browser,
// so the game runs over the table server's encrypted relay. TEST_DEADLINE_S raises the two-round
// deadline (default 180 s) for fault-injected servers. TEST_LIVENESS_MS tells the test that the
// server runs with that P2SKAT_LIVENESS_MS: closing a tab without a leave request must then end the
// game for the others with "lost connection". It also shortens the pages' relay keepalive (normally
// 30 s against the server's 120 s window) to a quarter of that window.
const assert = require('node:assert/strict');
const { execFileSync } = require('node:child_process');
const { chromium } = require('playwright-core');

const args = process.argv.slice(2);
const relay = args.includes('--relay') || process.env.TEST_RELAY === '1';
const url = args.find(arg => !arg.startsWith('--')) || 'http://localhost:8765';
const deadlineMs = Number(process.env.TEST_DEADLINE_S || 180) * 1000;
const livenessMs = Number(process.env.TEST_LIVENESS_MS || 0);
const keepaliveMs = livenessMs ? Math.max(500, Math.floor(livenessMs / 4)) : 0;
const PLAINTEXT_MARKERS = ['"type":"game"', '"type":"hello"', '"type":"ready"', '"type":"bye"', '"body"', '"version"', 'hello', 'Bert', 'Clara'];

// Acceptance check: every relay payload any page sent (POST bodies) or received (SSE) must be
// either a key frame or ciphertext. Nothing may contain a PeerMesh packet.
function checkRelayWire(wires) {
  const payloads = new Map();
  for (const { posts, events } of wires) {
    for (const body of posts) for (const frame of JSON.parse(body).frames) payloads.set(frame.payload, frame.seq);
    for (const data of events) {
      const event = JSON.parse(data);
      if (event.type === 'relay') payloads.set(event.payload, event.seq);
      else assert.ok(['players', 'closed'].includes(event.type), `Unexpected stream event ${event.type}`);
    }
  }
  let keys = 0;
  for (const [payload, seq] of payloads) {
    const text = Buffer.from(payload, 'base64url').toString('latin1');
    if (seq === 1) {
      const frame = JSON.parse(Buffer.from(payload, 'base64url').toString('utf8'));
      assert.deepEqual(Object.keys(frame).sort(), ['from', 'mac', 'mode', 'names', 'pub', 'room_id', 'to', 'type']);
      assert.equal(frame.type, 'key');
      assert.equal(frame.mode, 'relay');
      keys++;
      continue;
    }
    for (const marker of PLAINTEXT_MARKERS) assert.ok(!text.includes(marker), `Relay payload seq ${seq} contains plaintext ${marker}`);
    assert.throws(() => JSON.parse(Buffer.from(payload, 'base64url').toString('utf8')), undefined, 'Relay payload is readable JSON');
  }
  assert.equal(keys, 6, 'Each seat sends one key frame to each other seat');
  assert.ok(payloads.size > 100, 'Too few relay frames were captured');
  return payloads.size;
}

async function main() {
  const manual = process.env.TEST_MANUAL === '1';
  const remote = manual || relay || process.env.TEST_WEBRTC === '1' || Boolean(process.env.TEST_TURN_URL);
  const browser = await chromium.launch({
    executablePath: process.env.CHROMIUM_PATH || execFileSync('which', ['chromium'], { encoding: 'utf8' }).trim(),
    headless: true,
    // Actual same-browser testing uses no WebRTC candidate overrides.
    args: ['--no-sandbox', ...(remote ? ['--disable-features=WebRtcHideLocalIpsWithMdns'] : [])],
  });
  const sharedContext = remote ? null : await browser.newContext();
  const pages = [];
  const errors = [];
  try {
    for (let seat = 0; seat < 3; seat++) {
      const context = sharedContext || await browser.newContext();
      const page = await context.newPage();
      await page.setViewportSize(seat === 2 ? { width: 390, height: 844 } : { width: 1360, height: 900 });
      page.setDefaultTimeout(60000);
      page.on('pageerror', error => errors.push(`Seat ${seat}: ${error.message}`));
      if (process.env.TEST_CONSOLE) page.on('console', message => console.log(`[seat ${seat}] ${message.text()}`));
      await page.addInitScript(({ relayMode, keepaliveMs }) => {
        if (keepaliveMs) window.P2SKAT_KEEPALIVE_MS = keepaliveMs;
        window.testEvents = [];
        window.testFailures = [];
        window.testConnections = [];
        if (relayMode) {
          // Tor Browser has no WebRTC at all.
          delete window.RTCPeerConnection;
          delete window.webkitRTCPeerConnection;
          // Record everything this page exchanges with the relay.
          window.testWire = { posts: [], events: [], streams: 0, streamErrors: 0, postFailures: 0 };
          const nativeFetch = window.fetch;
          window.fetch = function (resource, options) {
            if (String(resource).endsWith('/relay')) window.testWire.posts.push(options.body);
            if (window.testBlockLeave && String(resource).endsWith('/leave')) return new Promise(() => {});
            const result = nativeFetch.apply(this, arguments);
            if (String(resource).endsWith('/relay')) result.catch(() => { window.testWire.postFailures++; });
            return result;
          };
          const NativeEventSource = window.EventSource;
          window.EventSource = class extends NativeEventSource {
            constructor(...args) {
              super(...args);
              window.testWire.streams++;
              this.addEventListener('message', ({ data }) => window.testWire.events.push(data));
              this.addEventListener('error', () => { window.testWire.streamErrors++; });
            }
          };
        } else {
          const NativeConnection = window.RTCPeerConnection;
          window.RTCPeerConnection = class extends NativeConnection {
            constructor(...args) {
              super(...args);
              window.testConnections.push(this);
            }
          };
        }
        const NativeWorker = window.Worker;
        window.Worker = class extends NativeWorker {
          constructor(...args) {
            super(...args);
            this.addEventListener('message', ({ data }) => {
              if (data.type === 'render') window.testEvents.push({ ...data.event, at: Date.now() });
              if (data.type === 'error') window.testFailures.push(data.message);
            });
          }
        };
      }, { relayMode: relay, keepaliveMs });
      await page.goto(url);
      pages.push(page);
      assert.deepEqual(await page.evaluate(() => {
        const ids = [...document.querySelectorAll('[id]')].map(node => node.id);
        return ids.filter((id, index) => ids.indexOf(id) !== index);
      }), [], 'Duplicate HTML IDs');
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false, 'Setup overflows viewport');
    }

    if (await pages[0].locator('#fullscreen').isVisible()) {
      await pages[0].bringToFront();
      await pages[0].locator('#fullscreen').click();
      await pages[0].waitForFunction(() => Boolean(document.fullscreenElement));
      assert.equal(await pages[0].locator('#fullscreen').getAttribute('aria-label'), 'Exit full screen');
      await pages[0].locator('#fullscreen').click();
      await pages[0].waitForFunction(() => !document.fullscreenElement);
    }
    await pages[0].locator('#manual-setup summary').click();
    await pages[0].locator('#self-check').click();
    const responsiveness = await pages[0].evaluate(async () => {
      const start = performance.now();
      await new Promise(resolve => setTimeout(resolve, 50));
      return performance.now() - start;
    });
    assert.ok(responsiveness < 2000, 'Crypto blocked the main thread');
    await pages[0].waitForFunction(() => /passed|verified|success/i.test(document.querySelector('#self-check-result').textContent), undefined, { timeout: 120000 });
    console.log('Crypto:', await pages[0].locator('#self-check-result').textContent());
    await pages[0].locator('#manual-setup summary').click();

    let invite;
    for (let seat = 0; seat < 3; seat++) {
      const page = pages[seat];
      if (manual) {
        await page.locator('#manual-setup summary').click();
        await page.locator('#seat').selectOption(String(seat));
        for (const [index, name] of ['Ada', 'Bert', 'Clara'].entries()) await page.locator(`#name-${index}`).fill(name);
      } else {
        if (seat > 0) await page.goto(invite);
        await page.locator('#player-name').fill(['Ada', 'Bert', 'Clara'][seat]);
      }
      await page.locator('.network-settings summary').click();
      if (relay) {
        assert.equal(await page.locator('#ice-settings').isVisible(), false, 'STUN/TURN settings shown in relay mode');
        assert.match(await page.locator('#relay-note').textContent(), /no WebRTC/);
      } else await page.locator('#stun').fill('');
      if (process.env.TEST_TURN_URL) {
        await page.locator('#turn-url').fill(process.env.TEST_TURN_URL);
        await page.locator('#turn-username').fill(process.env.TEST_TURN_USERNAME || '');
        await page.locator('#turn-credential').fill(process.env.TEST_TURN_CREDENTIAL || '');
        await page.locator('#relay-only').check();
      }
      if (manual) await page.locator('#prepare').click();
      else if (seat === 0) {
        await page.locator('#create-table').click();
        await page.waitForFunction(() => document.querySelector('#invite-link')?.value.includes('#room='));
        invite = await page.locator('#invite-link').inputValue();
        assert.equal(await page.locator('#open-player-tab').getAttribute('href'), invite);
        if (relay) {
          assert.match(invite, /#room=[\w-]+&k=[\w-]{43}&m=relay$/, 'Relay invite lacks its key or mode');
          assert.equal(await page.locator('#relay-warning').isVisible(), true);
        }
      } else await page.locator('#join-table').click();
    }
    const joinedAt = Date.now();

    for (const [lower, higher] of manual ? [[0, 1], [0, 2], [1, 2]] : []) {
      const out = `[data-peer="${higher}"] .signal-out`;
      await pages[lower].waitForFunction(selector => document.querySelector(selector)?.value.length > 0, out);
      await pages[higher].locator(`[data-peer="${lower}"] .signal-in`).fill(await pages[lower].locator(out).inputValue());
      await pages[higher].locator(`[data-peer="${lower}"] .signal-apply`).click();
      const answer = `[data-peer="${lower}"] .signal-out`;
      await pages[higher].waitForFunction(selector => document.querySelector(selector)?.value.length > 0, answer);
      await pages[lower].locator(`[data-peer="${higher}"] .signal-in`).fill(await pages[higher].locator(answer).inputValue());
      await pages[lower].locator(`[data-peer="${higher}"] .signal-apply`).click();
      console.log(`Paired seats ${lower + 1} and ${higher + 1}.`);
    }
    await Promise.all(pages.map(page => page.locator('#game').waitFor({ state: 'visible' })));
    console.log(manual ? 'All three manual WebRTC pairings ready.' : 'Invite assigned all three seats and connected automatically.');
    if (relay) {
      assert.deepEqual(await Promise.all(pages.map(page => page.evaluate(() => typeof RTCPeerConnection))), ['undefined', 'undefined', 'undefined']);
      console.log(`Relay mode: keys exchanged and mesh ready ${((Date.now() - joinedAt) / 1000).toFixed(1)} s after the last join.`);
    }
    if (!remote) {
      assert.deepEqual(await Promise.all(pages.map(page => page.evaluate(() => window.testConnections.length))), [0, 0, 0], 'Same-browser tabs attempted WebRTC network discovery');
      console.log('Three tabs connected locally without WebRTC overrides or pasted signals.');
    }
    if (process.env.TEST_TURN_URL) {
      for (const page of pages) {
        const types = await page.evaluate(async () => Promise.all(window.testConnections.map(async connection => {
          const stats = await connection.getStats();
          const transport = [...stats.values()].find(stat => stat.type === 'transport' && stat.selectedCandidatePairId);
          const pair = stats.get(transport?.selectedCandidatePairId);
          return stats.get(pair?.localCandidateId)?.candidateType;
        })));
        assert.deepEqual(types, ['relay', 'relay'], 'Relay-only test used a direct candidate');
      }
      console.log('Verified all six endpoints use TURN relay candidates.');
    }

    const handled = [0, 0, 0];
    const checkedLayouts = new Set();
    let verifiedRounds = 0;
    const deadline = Date.now() + deadlineMs;
    while (verifiedRounds < 2 && Date.now() < deadline) {
      const states = await Promise.all(pages.map(page => page.evaluate(() => ({
        choice: window.testEvents.filter(event => event.type === 'choice').at(-1),
        results: window.testEvents.filter(event => event.type === 'round_result'),
        failures: window.testFailures,
        round: window.testEvents.filter(event => event.type === 'round').at(-1)?.number,
      }))));
      assert.deepEqual(errors, [], 'Browser JavaScript error');
      for (const state of states) assert.deepEqual(state.failures, [], 'Worker failure');
      const complete = Math.min(...states.map(state => state.results.length));
      if (complete > verifiedRounds) {
        for (const state of states) assert.deepEqual(state.results[verifiedRounds].scores, states[0].results[verifiedRounds].scores, 'Peers disagree on scores');
        verifiedRounds++;
        console.log(`Round ${verifiedRounds}: all peers verified and scores agree`, states[0].results[verifiedRounds - 1].scores);
        const timing = await Promise.all(pages.map(page => page.evaluate(number => {
          const events = window.testEvents;
          const start = events.findIndex(event => event.type === 'round' && event.number === number);
          const hand = events.slice(start).find(event => event.type === 'hand');
          const result = events.slice(start).find(event => event.type === 'round_result');
          return { deal: hand.at - events[start].at, round: result.at - events[start].at };
        }, verifiedRounds)));
        console.log(`Round ${verifiedRounds} timing: deal ${(Math.max(...timing.map(t => t.deal)) / 1000).toFixed(2)} s, round ${(Math.max(...timing.map(t => t.round)) / 1000).toFixed(1)} s`);
        if (verifiedRounds === 2) break;
      }
      for (let seat = 0; seat < 3; seat++) {
        const { choice, round } = states[seat];
        if (!choice || choice.id <= handled[seat]) continue;
        const page = pages[seat];
        const action = name => page.locator(`[data-action="${name}"]`);
        const layoutKey = `${seat}:${choice.kind}`;
        if (!checkedLayouts.has(layoutKey)) {
          assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth || document.documentElement.scrollHeight > innerHeight + 1), false, `${layoutKey} does not fit the screen`);
          checkedLayouts.add(layoutKey);
        }
        if (choice.kind === 'continue') {
          if (round !== 1 || verifiedRounds < 1) continue;
          if (seat === 2) {
            if (handled[0] < states[0].choice?.id || handled[1] < states[1].choice?.id) continue;
            await page.waitForTimeout(300);
            const rounds = await Promise.all(pages.map(client => client.evaluate(() => window.testEvents.filter(event => event.type === 'round').at(-1)?.number)));
            assert.deepEqual(rounds, [1, 1, 1], 'Next round started without all players');
          }
          await action('continue').click();
        } else if (choice.kind === 'bid') {
          const bid = seat === 1 && !choice.is_responder && choice.next_bid === 18;
          await action(bid ? 'bid' : 'pass').click();
        } else if (choice.kind === 'pickup') {
          await action(round === 1 ? 'pickup' : 'hand').click();
        } else if (choice.kind === 'discard') {
          const viewport = page.viewportSize();
          await page.setViewportSize({ width: 390, height: 844 });
          assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth || document.documentElement.scrollHeight > innerHeight + 1), false, 'Twelve-card mobile hand does not fit the screen');
          await page.locator('#hand [data-card]').nth(0).click({ position: { x: 10, y: 15 } });
          await page.locator('#hand [data-card]').nth(1).click({ position: { x: 10, y: 15 } });
          await page.screenshot({ path: '/tmp/p2skat-discard-mobile.png', fullPage: true });
          await action('discard').click();
          await page.setViewportSize(viewport);
        } else if (choice.kind === 'contract') {
          await page.locator('#contract-type').selectOption(round === 1 ? 'grand' : 'null');
          await action('declare').click();
        } else if (choice.kind === 'card') {
          assert.match(await page.locator('#turn').textContent(), /your turn/i);
          if (round === 1 && choice.trick_num === 1) await page.screenshot({ path: `/tmp/p2skat-playing-${seat}.png`, fullPage: true });
          await page.locator('#hand [data-card]:not(:disabled)').first().click({ position: { x: 10, y: 15 } });
        } else throw new Error(`Unknown choice ${choice.kind}`);
        handled[seat] = choice.id;
      }
      await pages[0].waitForTimeout(30);
    }
    assert.equal(verifiedRounds, 2, 'Two complete rounds did not finish');
    for (const page of pages) {
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false, 'Table overflows viewport');
      const handSizes = await page.evaluate(() => window.testEvents.filter(event => event.type === 'choice' && event.kind === 'pickup').map(event => event.hand.length));
      assert.ok(handSizes.every(size => size === 10));
    }
    await pages[0].screenshot({ path: '/tmp/p2skat-browser-desktop.png', fullPage: true });
    await pages[2].screenshot({ path: '/tmp/p2skat-browser-mobile.png', fullPage: true });
    if (relay) {
      const wires = await Promise.all(pages.map(page => page.evaluate(() => window.testWire)));
      const frames = checkRelayWire(wires);
      console.log(`Checked ${frames} distinct relay payloads: only key frames and ciphertext, no plaintext packets.`);
      console.log('Per seat: relay POSTs', wires.map(wire => wire.posts.length), 'failed POSTs', wires.map(wire => wire.postFailures),
        'stream errors', wires.map(wire => wire.streamErrors), 'streams opened by hand', wires.map(wire => wire.streams - 1));
      // A player's tab disappears. With TEST_LIVENESS_MS, its leave request is blocked too, so only
      // the server's liveness timeout can tell the others.
      if (livenessMs) await pages[2].evaluate(() => { window.testBlockLeave = true; });
      const closedAt = Date.now();
      await pages[2].close();
      const expected = livenessMs ? /Clara lost connection/ : /Clara left/;
      await Promise.all(pages.slice(0, 2).map(page => page.waitForFunction(source => new RegExp(source).test(document.querySelector('#connection-state').textContent),
        expected.source, { timeout: livenessMs + 30000 })));
      console.log(`Closing a tab ended the game for the others after ${((Date.now() - closedAt) / 1000).toFixed(1)} s:`, await pages[0].locator('#connection-state').textContent());
      assert.deepEqual(errors, []);
      return;
    }
    await pages[0].locator('#leave').click();
    await Promise.all(pages.slice(1).map(page => page.waitForFunction(() => /left|ended|disconnect/i.test(document.querySelector('#connection-state').textContent))));
    assert.deepEqual(errors, []);
    console.log('Desktop/mobile layouts and leave propagation passed.');
  } catch (error) {
    for (const [seat, page] of pages.entries()) {
      console.error(`Seat ${seat}:`, await page.evaluate(() => ({
        status: document.querySelector('#connection-state')?.textContent,
        progress: document.querySelector('#room-progress')?.textContent,
        game: document.querySelector('#game-status')?.textContent,
        events: window.testEvents?.slice(-5),
        failures: window.testFailures,
      })).catch(() => null));
    }
    throw error;
  } finally {
    await browser.close();
  }
}

main().catch(error => { console.error(error); process.exitCode = 1; });
