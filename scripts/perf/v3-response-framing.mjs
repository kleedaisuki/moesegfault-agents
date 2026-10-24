// Policy experiment only: this does not replace the production Responses encoder.
import assert from 'node:assert/strict';
import { mkdir, writeFile } from 'node:fs/promises';
import { dirname, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

function frameBytes(text, sequence) {
  const type = 'response.output_text.delta';
  const event = { type, sequence_number: sequence, item_id: 'msg_response_12345678901234567890123456789012_0', output_index: 0, content_index: 0, delta: text, logprobs: [], obfuscation: 'a'.repeat(24) };
  return Buffer.byteLength(`event: ${type}\ndata: ${JSON.stringify(event)}\n\n`);
}

export function simulateFraming(arrivals, completedAt, policy) {
  const frames = [];
  let pending = [];
  let pendingBytes = 0;
  let lastEmit;
  const emit = at => {
    if (!pending.length) return;
    frames.push({ at_ms: at, text: pending.map(part => part.text).join(''), delays_ms: pending.map(part => at - part.at_ms) });
    pending = [];
    pendingBytes = 0;
    lastEmit = at;
  };
  for (const arrival of arrivals) {
    if (!arrival.text) continue;
    if (policy === 'timer' && pending.length && lastEmit + 50 <= arrival.at_ms) emit(lastEmit + 50);
    pending.push(arrival);
    pendingBytes += Buffer.byteLength(arrival.text);
    if (policy === 'immediate' || lastEmit === undefined || pendingBytes >= 1024 || arrival.at_ms - lastEmit >= 50) emit(arrival.at_ms);
  }
  if (policy === 'timer' && pending.length && lastEmit + 50 <= completedAt) emit(lastEmit + 50);
  emit(completedAt);
  const text = frames.map(frame => frame.text).join('');
  assert.equal(text, arrivals.map(arrival => arrival.text).join(''));
  const delays = frames.flatMap(frame => frame.delays_ms).sort((left, right) => left - right);
  return {
    frames: frames.length, text_bytes: Buffer.byteLength(text),
    delta_sse_body_bytes: frames.reduce((bytes, frame, sequence) => bytes + frameBytes(frame.text, sequence), 0),
    first_text_delay_ms: frames[0].at_ms - arrivals.find(arrival => arrival.text).at_ms,
    p50_fragment_delay_ms: delays[Math.ceil(delays.length * .5) - 1],
    p95_fragment_delay_ms: delays[Math.ceil(delays.length * .95) - 1],
    maximum_fragment_delay_ms: delays.at(-1),
    exact_text_preserved: true,
  };
}

export function framingExperiment() {
  const workloads = [
    { name: 'burst', arrivals: Array.from({ length: 2048 }, (_, index) => ({ at_ms: index * 2, text: 'text' })), completed_at_ms: 4096 },
    { name: 'steady', arrivals: Array.from({ length: 512 }, (_, index) => ({ at_ms: index * 20, text: 'fragment' })), completed_at_ms: 10240 },
    { name: 'utf8', arrivals: Array.from({ length: 512 }, (_, index) => ({ at_ms: index * 10, text: ['中文', '🌸', 'e\u0301', 'x'][index % 4] })), completed_at_ms: 5120 },
    { name: 'quiet_gap', arrivals: [{ at_ms: 0, text: 'first' }, { at_ms: 1, text: 'second' }, { at_ms: 3000, text: 'third' }], completed_at_ms: 10000 },
  ];
  return {
    schema: 1, node: process.version,
    workloads: workloads.map(workload => ({ name: workload.name, policies: Object.fromEntries(['immediate', 'callback', 'timer'].map(policy => [policy, simulateFraming(workload.arrivals, workload.completed_at_ms, policy)])) })),
    notes: [
      'Synthetic deterministic framing-policy simulation, not executed production encoder or deployment performance.',
      'Each policy uses identical text, sequence fields and a fixed 24-character padding sample to isolate frame-count overhead.',
      'A 1 KiB/50 ms callback-only threshold is not a 50 ms delivery bound when a provider becomes quiet; a timer is required.',
      'First nonempty delta is emitted immediately. Completion flushes remaining text. Lifecycle/tool events must remain separate ordered barriers.',
      'Delay is measured per original provider fragment, not weighted by characters. Compression, CPU, storage and transport headers are excluded.',
    ],
  };
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  const report = framingExperiment();
  const destination = resolve('.cache/perf/v3/framing-policy.json');
  await mkdir(dirname(destination), { recursive: true });
  await writeFile(destination, `${JSON.stringify(report, null, 2)}\n`);
  process.stdout.write(`${JSON.stringify(report, null, 2)}\n`);
}
