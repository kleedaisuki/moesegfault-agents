import { ModelOperation as RustModelOperation } from '../index.js';
export * from './workspace-blob-shim.mjs';
export { default } from './workspace-blob-shim.mjs';

// Fixture-only actor seed/inspection. The distributable Worker cannot import it.
export class ModelOperation extends RustModelOperation {
  constructor(ctx, env) { super(ctx, env); this.fixtureStorage = ctx.storage; }
  async fetch(request) {
    const path = new URL(request.url).pathname;
    if (path === '/__test/execution-receipt')
      return Response.json(await this.fixtureStorage.get('compact.receipt.v1'));
    if (path !== '/__test/execution-seed') return super.fetch(request);
    const { receipt, prepared } = await request.json();
    const bytes = new TextEncoder().encode(JSON.stringify(prepared));
    const generation = 'fixture-lease-input';
    await this.fixtureStorage.put(`compact.input.v1:${generation}:0`, Array.from(bytes));
    await this.fixtureStorage.put('compact.input.v1', { generation, chunks: 1, bytes: bytes.length });
    await this.fixtureStorage.put('compact.receipt.v1', receipt);
    return Response.json({ seeded: true });
  }
}
