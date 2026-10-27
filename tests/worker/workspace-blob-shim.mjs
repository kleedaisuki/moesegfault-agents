import RecoveryWorker from './chat-recovery-shim.mjs';
import RustWorker, { Session as RustSession } from '../index.js';
export * from './chat-recovery-shim.mjs';

// This module exists only in the Miniflare fixture module graph. Neither the
// release bundle nor production routes expose its storage/control operations.
export class Session extends RustSession {
  constructor(ctx, env) {
    super(ctx, env);
    this.fixtureStorage = ctx.storage;
  }

  async fetch(request) {
    if (new URL(request.url).pathname === '/__test/workspace-blob-seed')
      return Response.json({ manifest: await this.fixtureStorage.get('workspace:manifest'),
        pending: await this.fixtureStorage.get('workspace:seed-pending') });
    if (new URL(request.url).pathname !== '/__test/workspace-blob-response') return super.fetch(request);
    const { responseId, archive } = await request.json();
    const key = `response:${responseId}`;
    const index = await this.fixtureStorage.get(key);
    if (!index) throw new Error('Seed requires an actually completed Rust response');
    const parts = await Promise.all(Array.from({ length: index.chunks }, (_, n) =>
      this.fixtureStorage.get(`${key}:${index.generation}:${n}`)));
    const bytes = new Uint8Array(parts.reduce((size, part) => size + part.length, 0));
    let offset = 0;
    for (const part of parts) { bytes.set(part, offset); offset += part.length; }
    const value = JSON.parse(new TextDecoder().decode(bytes));
    if (value.id !== responseId || value.status !== 'completed' || !value.branch_point)
      throw new Error('Seed requires a retained immutable response boundary');
    value.branch_point.workspace = { kind: 'archive', archive };
    const encoded = new TextEncoder().encode(JSON.stringify(value));
    const generation = `fixture-${crypto.randomUUID()}`;
    const chunkBytes = 256 * 1024;
    const chunks = Math.ceil(encoded.length / chunkBytes);
    for (let n = 0; n < chunks; n++)
      await this.fixtureStorage.put(`${key}:${generation}:${n}`, Array.from(encoded.subarray(n * chunkBytes, (n + 1) * chunkBytes)));
    await this.fixtureStorage.put(key, { generation, chunks, bytes: encoded.length });
    for (let n = 0; n < index.chunks; n++) await this.fixtureStorage.delete(`${key}:${index.generation}:${n}`);
    return Response.json({ seeded: true, source_session_id: value.branch_point.source_session_id });
  }
}

export default class extends RecoveryWorker {
  async fetch(request) {
    if (new URL(request.url).pathname !== '/__test/workspace-blob-gc') return super.fetch(request);
    const pending = [];
    const ctx = { waitUntil(promise) { pending.push(promise); } };
    await RustWorker.prototype.scheduled.call({ env: this.env, ctx },
      { scheduledTime: Date.now(), cron: '* * * * *', noRetry() {} });
    await Promise.all(pending);
    return Response.json({ swept: true });
  }
}
