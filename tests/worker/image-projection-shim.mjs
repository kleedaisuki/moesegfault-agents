import WorkspaceWorker, { Session as WorkspaceSession } from './workspace-blob-shim.mjs';
export * from './workspace-blob-shim.mjs';
export default WorkspaceWorker;

async function load(storage, id) {
  const key = `response:${id}`;
  const index = await storage.get(key);
  if (!index) throw Error('Image seed requires a real retained Rust Response');
  const parts = await Promise.all(Array.from({ length: index.chunks }, (_, n) =>
    storage.get(`${key}:${index.generation}:${n}`)));
  return { key, index, value: JSON.parse(new TextDecoder().decode(Uint8Array.from(parts.flat()))) };
}

export class Session extends WorkspaceSession {
  constructor(ctx, env) { super(ctx, env); this.imageStorage = ctx.storage; }

  async fetch(request) {
    const pathname = new URL(request.url).pathname;
    if (!['/__test/image-projection-seed', '/__test/image-projection-inspect'].includes(pathname))
      return super.fetch(request);
    const { responseId, assets } = await request.json();
    const { key, index, value } = await load(this.imageStorage, responseId);
    if (pathname.endsWith('inspect')) {
      const inlineItem = item => item?.type === 'image_generation_call' && item.result !== null;
      return Response.json({ canonical_bytes: new TextEncoder().encode(JSON.stringify(value)).length,
        assets: value.images,
        inline_final: value.output.some(inlineItem) || value.events.some(event => inlineItem(event.item)
          || event.response?.output?.some(inlineItem)),
        inline_partial: value.events.some(event => event.type === 'response.image_generation_call.partial_image'
          && event.partial_image_b64 !== null) });
    }
    if (value.id !== responseId || value.status !== 'completed' || !value.branch_point || !value.request.store)
      throw Error('Image seed requires a genuinely completed retained Rust Response');
    if (!Array.isArray(assets) || assets.length < 1 || assets.length > 4)
      throw Error('Image seed accepts one checked final and at most three checked previews');
    const final = assets.find(asset => asset.partial_index === null);
    if (!final || assets.some(asset => asset.item_id !== final.item_id)) throw Error('Invalid image slots');
    const item = { id: final.item_id, type: 'image_generation_call', status: 'completed', result: null,
      action: 'generate', output_format: 'png', background: 'opaque', size: '1024x1024' };
    const terminalIndex = value.events.findLastIndex(event => event.type === 'response.completed');
    if (terminalIndex < 0) throw Error('Image seed needs the real canonical completion');
    const body = structuredClone(value.events[terminalIndex].response);
    const outputIndex = value.output.length;
    value.output.push(item);
    value.images = assets;
    body.output = structuredClone(value.output);
    const events = value.events.slice(0, terminalIndex);
    let sequence = (events.at(-1)?.sequence_number ?? -1) + 1;
    const append = (type, fields) => events.push({ type, sequence_number: sequence++, ...fields });
    append('response.output_item.added', { output_index: outputIndex,
      item: { ...item, status: 'in_progress' } });
    append('response.image_generation_call.in_progress', { output_index: outputIndex, item_id: item.id });
    for (const asset of assets.filter(asset => asset.partial_index !== null))
      append('response.image_generation_call.partial_image', { output_index: outputIndex, item_id: item.id,
        partial_image_index: asset.partial_index, partial_image_b64: null });
    append('response.image_generation_call.completed', { output_index: outputIndex, item_id: item.id });
    append('response.output_item.done', { output_index: outputIndex, item });
    append('response.completed', { response: body });
    value.events = events;
    value.next_sequence = sequence;
    value.event_bytes = events.reduce((bytes, event) => bytes + new TextEncoder().encode(JSON.stringify(event)).length, 0);
    const bytes = new TextEncoder().encode(JSON.stringify(value));
    const generation = `fixture-image-${crypto.randomUUID()}`;
    const chunkBytes = 256 * 1024;
    const chunks = Math.ceil(bytes.length / chunkBytes);
    for (let n = 0; n < chunks; n++)
      await this.imageStorage.put(`${key}:${generation}:${n}`, Array.from(bytes.subarray(n * chunkBytes, (n + 1) * chunkBytes)));
    await this.imageStorage.put(key, { generation, chunks, bytes: bytes.length });
    for (let n = 0; n < index.chunks; n++) await this.imageStorage.delete(`${key}:${index.generation}:${n}`);
    return Response.json({ seeded: true, canonical_bytes: bytes.length });
  }
}
