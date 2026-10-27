import assert from 'node:assert/strict';
import OpenAI from 'openai';
import { createBrowserPrincipal } from './browser-principal.mjs';

export async function runDeepseekAutomaticCompactJourney({ mf, encryptionKey, request, provider, configure }) {
  const registry = [{ provider:'deepseek', protocol:'responses', model:'deepseek-flash', context_window:32768, max_output_tokens:64 }];
  const prices = { 'deepseek/deepseek-flash': { input_microusd_per_million:150000,
    cached_input_microusd_per_million:3000, output_microusd_per_million:600000, revision:'automatic-purpose-v1' } };
  const db = await configure({ MODEL_REGISTRY_JSON:JSON.stringify(registry), MODEL_PRICING_JSON:JSON.stringify(prices),
    DEEPSEEK_API_KEY:'synthetic-purpose-specific-deepseek-key' });
  provider.state.enabled = true;
  try {
    const principal = await createBrowserPrincipal({ mf, db, encryptionKey, owner:'automatic-deepseek-compaction-owner' });
    await principal.browser('GET','/v1/account');
    const agent = await request('POST','/v1/agents',{ name:'Automatic compaction purpose',
      model:registry[0], instructions:'Keep SERVER_AUTO and the historical developer policy.',
      tools:[],workspace:false },201,principal.apiKey);
    const sdk = new OpenAI({ apiKey:principal.apiKey, baseURL:'https://agents.test/v1', maxRetries:0,
      fetch:(input,init)=>mf.dispatchFetch(input,init) });
    const input = [{ role:'developer',content:'Keep DEVELOPER_AUTO.' },
      { role:'user',content:'A correction: preserve the goal rather than starting again.' }];
    const response = await (await sdk.responses.create({ model:agent.id, input,
      instructions:'Keep CUSTOM_AUTO.', reasoning:{effort:'none'}, max_output_tokens:64,
      context_management:[{type:'compaction',compact_threshold:1}] },
      {headers:{'Idempotency-Key':'automatic-deepseek-purpose-once'}}).asResponse()).json();
    assert.equal(response.status,'completed',JSON.stringify(response));
    assert.equal(provider.state.compactions.length,1);
    assert.equal(provider.state.generations.length,1);
    const compact = provider.state.compactions[0];
    assert.equal(compact.thinking.type,'disabled');
    assert.equal(compact.max_tokens,64);
    assert.equal(compact.tools,undefined);
    const source = JSON.stringify(compact.messages);
    for (const marker of ['SERVER_AUTO','DEVELOPER_AUTO','CUSTOM_AUTO','A correction: preserve']) assert.ok(source.includes(marker),marker);
    const next = JSON.stringify(provider.state.generations[0]);
    assert.ok(next.includes('Preserved goal, custom tool result and correction'));
    for (const marker of ['SERVER_AUTO','DEVELOPER_AUTO','CUSTOM_AUTO']) assert.ok(next.includes(marker),marker);
    assert.equal(provider.state.generations[0].reasoning.effort,'none');
    const item=response.output.find(item=>item.type==='compaction');
    assert.ok(item?.encrypted_content);
    assert.equal(response.output.filter(item=>item.type==='compaction').length,1);
    const stored = await (await sdk.responses.retrieve(response.id).asResponse()).json();
    assert.deepEqual(stored,response);
    assert.equal(provider.state.compactions.length,1,'GET cannot buy a second summary');
    const reservations=await db.prepare('SELECT state FROM usage_reservations WHERE account_id=? AND idempotency_key LIKE ?').bind(principal.owner,'completion:%').all();
    assert.equal(reservations.results.length,2);
    assert.ok(reservations.results.every(row=>row.state==='settled'));
    const compactLedger=await db.prepare("SELECT reserved_microusd FROM usage_reservations WHERE account_id=? AND idempotency_key LIKE '%compact:0'").bind(principal.owner).first();
    assert.ok(compactLedger && compactLedger.reserved_microusd<1000,'Summary escrow uses exact rendered source, not whole context window');
    const usage=response.usage ?? response.agents_usage;
    assert.equal(usage.output_tokens_details.reasoning_tokens,0);
    console.log(JSON.stringify({ execution:'automatic_deepseek_compaction_purpose', status:'passed',
      compactions:1,generations:1,external_calls:0,public_response:response.id,settled_model_operations:2,
      transport:'anthropic',thinking:'disabled',protected_roles_preserved:true,raw_get_immutable:true }));
  } finally { provider.state.enabled=false; await configure(); }
}
