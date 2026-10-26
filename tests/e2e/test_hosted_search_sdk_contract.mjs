import assert from 'node:assert/strict';
import test from 'node:test';
import { main, searchRequest, summarizeSearch, verifySearchPriceSource } from './hosted-search-sdk.mjs';

test('new live Search body permits one official-source search and no compute or follow-up fetch', () => {
  const body = searchRequest('owned-agent');
  assert.equal(body.max_tool_calls, 1); assert.equal(body.max_output_tokens, 256);
  assert.equal(body.tool_choice, undefined);
  assert.deepEqual(body.include, ['web_search_call.action.sources']);
  assert.deepEqual(body.tools, [{ type: 'web_search', search_context_size: 'low', filters: { allowed_domains: ['doc.rust-lang.org'] } }]);
});

test('live Search evidence requires native real sources, phase order and the direct 750 tariff', () => {
  const url = 'https://doc.rust-lang.org/book/ch04-00-understanding-ownership.html';
  const response = { status: 'completed', output: [{ id: 'ws-owned', type: 'web_search_call', status: 'completed', action: { type: 'search', query: 'Rust ownership', sources: [{ type: 'url', url }] } },
    { type: 'message', content: [{ type: 'output_text', text: 'See ' + url }] }] };
  const events = ['in_progress', 'searching', 'completed'].map(phase => ({ type: 'response.web_search_call.' + phase, item_id: 'ws-owned' }));
  assert.equal(summarizeSearch(response, events).sources.length, 1);
  assert.equal(summarizeSearch(response, events).answerMentionsKnownSource, true);
  assert.throws(() => summarizeSearch({ ...response, output: [{ ...response.output[0], action: { ...response.output[0].action, sources: [] } }] }, events));
  assert.throws(() => summarizeSearch(response, events.slice(1)));
  const source = 'pub const PRICE_REVISION: &str = "deepseek-direct-search-fetch-2026-10-26";\npub const PAID_UNIT_MICRODOLLARS: u64 = 750;';
  assert.equal(verifySearchPriceSource(source).successfulRequestMicroUSD, 750);
  assert.throws(() => verifySearchPriceSource(source.replace('750', '0')));
});

test('live Search rejects ungated calls before network or key access', async () => {
  await assert.rejects(main([], {}), /exact_search_assembly_source_required/);
  await assert.rejects(main(['--source', 'a'.repeat(40)], {}), /single_new_paid_search_gate_required/);
  await assert.rejects(main(['--source', 'a'.repeat(40), '--allow-paid-search'], {}), /owned_platform_key_missing/);
});
