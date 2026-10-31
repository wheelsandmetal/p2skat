// Run against the built, served web/ directory with playwright-core on NODE_PATH.
const assert = require('node:assert/strict');
const { execFileSync } = require('node:child_process');
const { chromium } = require('playwright-core');

async function main() {
  const browser = await chromium.launch({
    executablePath: process.env.CHROMIUM_PATH || execFileSync('which', ['chromium'], { encoding: 'utf8' }).trim(),
    headless: true,
    // Local multicast is unavailable in some headless environments.
    args: ['--no-sandbox', '--disable-features=WebRtcHideLocalIpsWithMdns'],
  });
  const pages = [];
  const errors = [];
  try {
    for (let seat = 0; seat < 3; seat++) {
      const context = await browser.newContext({ viewport: seat === 2 ? { width: 390, height: 844 } : { width: 1360, height: 900 } });
      const page = await context.newPage();
      page.setDefaultTimeout(60000);
      page.on('pageerror', error => errors.push(`Seat ${seat}: ${error.message}`));
      await page.addInitScript(() => {
        window.testEvents = [];
        window.testFailures = [];
        window.testConnections = [];
        const NativeConnection = window.RTCPeerConnection;
        window.RTCPeerConnection = class extends NativeConnection {
          constructor(...args) {
            super(...args);
            window.testConnections.push(this);
          }
        };
        const NativeWorker = window.Worker;
        window.Worker = class extends NativeWorker {
          constructor(...args) {
            super(...args);
            this.addEventListener('message', ({ data }) => {
              if (data.type === 'render') window.testEvents.push(data.event);
              if (data.type === 'error') window.testFailures.push(data.message);
            });
          }
        };
      });
      await page.goto(process.argv[2] || 'http://localhost:8765');
      pages.push(page);
      assert.deepEqual(await page.evaluate(() => {
        const ids = [...document.querySelectorAll('[id]')].map(node => node.id);
        return ids.filter((id, index) => ids.indexOf(id) !== index);
      }), [], 'Duplicate HTML IDs');
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false, 'Setup overflows viewport');
    }

    await pages[0].locator('#self-check').click();
    const responsiveness = await pages[0].evaluate(async () => {
      const start = performance.now();
      await new Promise(resolve => setTimeout(resolve, 50));
      return performance.now() - start;
    });
    assert.ok(responsiveness < 2000, 'Crypto blocked the main thread');
    await pages[0].waitForFunction(() => /passed|verified|success/i.test(document.querySelector('#self-check-result').textContent), undefined, { timeout: 120000 });
    console.log('Crypto:', await pages[0].locator('#self-check-result').textContent());

    for (let seat = 0; seat < 3; seat++) {
      const page = pages[seat];
      await page.locator('#seat').selectOption(String(seat));
      for (const [index, name] of ['Ada', 'Bert', 'Clara'].entries()) await page.locator(`#name-${index}`).fill(name);
      await page.locator('.network-settings summary').click();
      await page.locator('#stun').fill('');
      if (process.env.TEST_TURN_URL) {
        await page.locator('#turn-url').fill(process.env.TEST_TURN_URL);
        await page.locator('#turn-username').fill(process.env.TEST_TURN_USERNAME || '');
        await page.locator('#turn-credential').fill(process.env.TEST_TURN_CREDENTIAL || '');
        await page.locator('#relay-only').check();
      }
      await page.locator('#prepare').click();
    }

    for (const [lower, higher] of [[0, 1], [0, 2], [1, 2]]) {
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
    console.log('All three WebRTC pairings ready.');
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
    let verifiedRounds = 0;
    const deadline = Date.now() + 180000;
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
        if (verifiedRounds === 2) break;
      }
      for (let seat = 0; seat < 3; seat++) {
        const { choice, round } = states[seat];
        if (!choice || choice.id <= handled[seat]) continue;
        const page = pages[seat];
        const action = name => page.locator(`[data-action="${name}"]`);
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
          await page.locator('#hand [data-card]').nth(0).click();
          await page.locator('#hand [data-card]').nth(1).click();
          await action('discard').click();
        } else if (choice.kind === 'contract') {
          await page.locator('#contract-type').selectOption(round === 1 ? 'grand' : 'null');
          await action('declare').click();
        } else if (choice.kind === 'card') {
          assert.match(await page.locator('#turn').textContent(), /your turn/i);
          await page.locator('#hand [data-card]:not(:disabled)').first().click();
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
    await pages[0].locator('#leave').click();
    await Promise.all(pages.slice(1).map(page => page.waitForFunction(() => /left|ended|disconnect/i.test(document.querySelector('#connection-state').textContent))));
    assert.deepEqual(errors, []);
    console.log('Desktop/mobile layouts and leave propagation passed.');
  } catch (error) {
    for (const [seat, page] of pages.entries()) {
      console.error(`Seat ${seat}:`, await page.evaluate(() => ({
        status: document.querySelector('#connection-state')?.textContent,
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
