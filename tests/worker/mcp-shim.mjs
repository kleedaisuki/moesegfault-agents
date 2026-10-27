import RecoveryWorker from './chat-recovery-shim.mjs';
import { Session as RustSession } from '../index.js';
export * from './chat-recovery-shim.mjs';

// Test-only module graph: no production route can inspect private actor state.
export class Session extends RustSession {
  constructor(ctx, env) {
    super(ctx, env);
    this.fixtureStorage = ctx.storage;
    this.fixtureEnv = env;
  }

  async readFixtureChunks(key) {
    const index = await this.fixtureStorage.get(key);
    if (!index) return null;
    if (index.bytes > 16 * 1024 * 1024) throw new Error('Oversized fixture state');
    const parts = await Promise.all(Array.from({ length: index.chunks }, (_, n) =>
      this.fixtureStorage.get(`${key}:${index.generation}:${n}`)));
    return JSON.parse(new TextDecoder().decode(Uint8Array.from(parts.flat())));
  }

  async openOperation(token, owner, key) {
    if (typeof token !== 'string') throw new Error('Operation must be encrypted');
    const decode = value => Uint8Array.from(atob(value.replaceAll('-', '+').replaceAll('_', '/')), c => c.charCodeAt(0));
    const bytes = decode(token);
    const aes = await crypto.subtle.importKey('raw', decode(this.fixtureEnv.DATA_ENCRYPTION_KEY), 'AES-GCM', false, ['decrypt']);
    const plain = await crypto.subtle.decrypt({ name: 'AES-GCM', iv: bytes.slice(1, 13),
      additionalData: new TextEncoder().encode(`mcp-operation:v1:${owner}:${key}`) }, aes, bytes.slice(13));
    return JSON.parse(new TextDecoder().decode(plain));
  }

  async fetch(request) {
    const url = new URL(request.url);
    if (url.pathname !== '/__test/mcp-credentials') return super.fetch(request);
    const id = url.searchParams.get('response');
    const response = await this.readFixtureChunks(`response:${id}`);
    if (!response) return Response.json({ exists: false });
    const cache = await this.readFixtureChunks(`mcp-bindings:${id}`);
    const owner = response.mcp_bindings?.servers?.[0]?.owner ?? cache?.bindings?.servers?.[0]?.owner;
    const operations = await Promise.all((response.mcp_operation_keys ?? []).map(async key =>
      this.openOperation(await this.readFixtureChunks(key), owner, key)));
    const cleared = binding => (binding?.servers ?? []).every(server => server.headers_encrypted === '');
    return Response.json({ exists: true,
      canonical_cleared: cleared(response.mcp_bindings) && (response.mcp_approvals ?? []).every(action => action.intent.server.headers_encrypted === ''),
      cache_cleared: cleared(cache?.bindings),
      operations_cleared: operations.every(operation => operation?.intent?.server?.headers_encrypted === ''),
    });
  }
}

export default RecoveryWorker;
