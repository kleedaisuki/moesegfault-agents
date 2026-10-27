// Untrusted program data runs in a fresh Worker Loader isolate with no bindings
// and globalOutbound:null. This module never executes a tool or grants authority.
// The Rust actor validates and commits every proposed call before its side effect.
// JavaScript byte limits are defense in depth, not a trusted resource boundary:
// the actor must enforce native byte caps even if a program mutates intrinsics.
const primordials = Object.freeze({
  Promise, Date, random: Math.random.bind(Math), stringify: JSON.stringify.bind(JSON),
  parse: JSON.parse.bind(JSON), keys: Object.keys.bind(Object),
  hasOwn: Function.call.bind(Object.prototype.hasOwnProperty),
  Response, TextEncoder, TextDecoder, ReadableStream, Error,
});

for (const name of ['console', 'reportError', 'performance', 'crypto', 'caches', 'scheduler',
  'setTimeout', 'setInterval', 'clearTimeout', 'clearInterval']) {
  Object.defineProperty(globalThis, name, { value: undefined, writable: false, configurable: false });
}

// The renderer replaces the marker with a JSON string literal, not source text.
// AsyncFunction's body has no module lexical scope and is never run at startup.
const program = new (Object.getPrototypeOf(async function () {}).constructor)(
  'tools', /*__MAGENTS_PROGRAM_BODY__*/
);

const BYTE_LIMIT = 1024 * 1024;
const ENTROPY_LIMIT = 64 * 1024;
const MAX_ENTROPY = 4096;
const MAX_NEW_CALLS = 64;
const MAX_CALLS = 256;
const MAX_MICROTASK_TURNS = 4096;
const ENABLE_REPLAY_PROFILE = false;
const ENABLE_JSON_LIVE_PROFILE = false;
const MAX_CHANNEL_FRAMES = MAX_CALLS * 2 + 2;
let activeProgram;
let duplexStarted = false;

function fail(code) {
  const error = new primordials.Error(code);
  error.programError = code;
  throw error;
}

function json(value, limit, code) {
  let encoded;
  try { encoded = primordials.stringify(value); } catch { fail(code); }
  if (encoded === undefined || new primordials.TextEncoder().encode(encoded).length > limit) fail(code);
  return encoded;
}

function copy(value, limit, code) {
  return primordials.parse(json(value, limit, code));
}

function canonical(value) {
  if (Array.isArray(value)) return `[${value.map(canonical).join(',')}]`;
  if (value !== null && typeof value === 'object') {
    return `{${primordials.keys(value).sort().map(key => `${primordials.stringify(key)}:${canonical(value[key])}`).join(',')}}`;
  }
  return primordials.stringify(value);
}

function validPath(path) {
  return Array.isArray(path) && path.length > 0 && path.length <= 9 && path.every(part =>
    typeof part === 'string' && /^[A-Za-z0-9_-]{1,256}$/.test(part));
}

function entropyValue(entry) {
  if (!entry || typeof entry !== 'object' || !Array.isArray(entry.args) || entry.args.length !== 0) return false;
  if (entry.kind === 'random') return typeof entry.value === 'number' && entry.value >= 0 && entry.value < 1;
  if (entry.kind === 'date_call') return typeof entry.value === 'string' && entry.value.length <= 128;
  return ['date_now', 'date_construct'].includes(entry.kind) && Number.isSafeInteger(entry.value)
    && Math.abs(entry.value) <= 8_640_000_000_000_000;
}

function entropyTape(input) {
  const old = copy(input ?? [], ENTROPY_LIMIT, 'program_entropy_limit');
  if (!Array.isArray(old) || old.length > MAX_ENTROPY || !old.every(entropyValue)) fail('invalid_program_entropy');
  const values = old.slice();
  let consumed = 0;
  function observe(kind, observeValue) {
    const entry = values[consumed];
    if (entry) {
      if (entry.kind !== kind) fail('program_replay_conflict');
      consumed++;
      return entry.value;
    }
    const next = { kind, args: [], value: observeValue() };
    if (!entropyValue(next) || values.length >= MAX_ENTROPY) fail('program_entropy_limit');
    values.push(next);
    json(values, ENTROPY_LIMIT, 'program_entropy_limit');
    consumed++;
    return next.value;
  }
  return {
    values, observe,
    complete() { if (consumed < old.length) fail('program_replay_conflict'); },
  };
}

function installEntropy(tape) {
  const NativeDate = primordials.Date;
  const nativeNow = NativeDate.now.bind(NativeDate);
  function ObservedDate(...args) {
    if (!new.target) return tape.observe('date_call', () => NativeDate());
    const actual = args.length ? args : [tape.observe('date_construct', nativeNow)];
    return Reflect.construct(NativeDate, actual, new.target);
  }
  ObservedDate.prototype = NativeDate.prototype;
  Object.defineProperty(ObservedDate.prototype, 'constructor', { value: ObservedDate, writable: false });
  Object.defineProperties(ObservedDate, {
    now: { value: () => tape.observe('date_now', nativeNow) },
    parse: { value: NativeDate.parse.bind(NativeDate) },
    UTC: { value: NativeDate.UTC.bind(NativeDate) },
  });
  Object.defineProperty(globalThis, 'Date', { value: ObservedDate, writable: false, configurable: false });
  Object.defineProperty(Math, 'random', { value: () => tape.observe('random', primordials.random),
    writable: false, configurable: false });
}

function inputState(input) {
  if (!input || typeof input !== 'object' || Array.isArray(input)) fail('invalid_program_input');
  json(input, BYTE_LIMIT, 'program_input_limit');
  const maximum = input.max_calls ?? 64;
  const outputLimit = input.max_output_bytes ?? BYTE_LIMIT;
  if (!Number.isInteger(maximum) || maximum < 1 || maximum > MAX_CALLS
    || !Number.isInteger(outputLimit) || outputLimit < 1 || outputLimit > BYTE_LIMIT) fail('invalid_program_limits');
  const known = input.known ?? [];
  if (!Array.isArray(known) || known.length > maximum) fail('invalid_program_journal');
  const resolved = [];
  known.forEach((record, sequence) => {
    if (!record || record.sequence !== sequence || !validPath(record.target)
      || !primordials.hasOwn(record, 'arguments')) fail('invalid_program_journal');
    json(record.arguments, BYTE_LIMIT, 'program_argument_limit');
    if (!primordials.hasOwn(record, 'result')) {
      if (record.resolution_order !== undefined) fail('invalid_program_journal');
      return;
    }
    json(record.result, BYTE_LIMIT, 'program_result_limit');
    if (!Number.isInteger(record.resolution_order) || record.resolution_order < 0) fail('invalid_program_journal');
    resolved.push(record);
  });
  resolved.sort((left, right) => left.resolution_order - right.resolution_order);
  resolved.forEach((record, order) => {
    if (record.resolution_order !== order) fail('invalid_program_journal');
  });
  return { maximum, outputLimit, known, resolved };
}

function toolTree(descriptors, invoke) {
  if (!Array.isArray(descriptors) || descriptors.length > MAX_CALLS) fail('invalid_program_tools');
  const root = { children: new Map() };
  for (const descriptor of descriptors) {
    if (!descriptor || !validPath(descriptor.path) || !['function', 'custom'].includes(descriptor.kind)) fail('invalid_program_tools');
    let node = root;
    for (const part of descriptor.path) {
      if (!node.children.has(part)) node.children.set(part, { children: new Map() });
      node = node.children.get(part);
    }
    if (node.descriptor) fail('invalid_program_tools');
    node.descriptor = descriptor;
  }
  function render(node) {
    const descriptor = node.descriptor;
    const value = descriptor
      ? argumentsValue => invoke(descriptor.path, descriptor.kind, argumentsValue)
      : Object.create(null);
    for (const [part, child] of node.children) {
      Object.defineProperty(value, part, { enumerable: true, value: render(child) });
    }
    return Object.freeze(value);
  }
  return render(root);
}

function liveProgram(input) {
  const policy = inputState(input);
  if (policy.known.length || (input.entropy?.length ?? 0)) fail('invalid_live_program_input');
  const state = {
    id: input.program_id, policy, descriptors: input.tools ?? [], calls: [],
    pending: new Map(), resolution: 0, version: 0, finished: false,
    cancelled: false, error: undefined, result: '', text: [], media: [],
  };
  state.signature = canonical({ tools: state.descriptors,
    max_calls: policy.maximum, max_output_bytes: policy.outputLimit });
  function text(value) {
    state.text.push(typeof value === 'string' ? value : json(value, policy.outputLimit, 'program_output_limit'));
    json(state.text, policy.outputLimit, 'program_output_limit');
    state.version++;
  }
  function image(value) {
    if (typeof value !== 'string' || !/^data:image\/(png|jpeg|webp);base64,(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/.test(value)
      || value.endsWith(',')) fail('invalid_program_image');
    state.media.push({ type: 'image', image_url: value });
    json(state.media, policy.outputLimit, 'program_output_limit');
    state.version++;
  }
  const tools = toolTree(state.descriptors, (target, kind, value) => {
    if (state.cancelled) fail('program_cancelled');
    if (state.calls.length >= policy.maximum || state.pending.size >= MAX_NEW_CALLS) fail('program_call_limit');
    if (kind === 'function' && (value === null || typeof value !== 'object' || Array.isArray(value))
      || kind === 'custom' && typeof value !== 'string') fail('invalid_program_arguments');
    const args = copy(value, BYTE_LIMIT, 'program_argument_limit');
    const call = { sequence: state.calls.length, target: target.slice(), arguments: args };
    state.calls.push(call);
    state.version++;
    return new primordials.Promise(resolve => {
      state.pending.set(call.sequence, { resolve });
    });
  });
  Object.defineProperties(globalThis, {
    text: { value: text, writable: false, configurable: false },
    image: { value: image, writable: false, configurable: false },
  });
  primordials.Promise.resolve().then(() => program(tools)).then(value => {
    if (state.cancelled) return;
    try {
      state.result = state.text.length ? state.text.join('\n')
        : value === undefined ? '' : typeof value === 'string' ? value
          : json(value, policy.outputLimit, 'program_output_limit');
      json(state.result, policy.outputLimit, 'program_output_limit');
    } catch (error) { state.error = safeError(error); }
    state.finished = true;
    state.version++;
  }, error => {
    if (!state.cancelled) state.error = safeError(error);
    state.finished = true;
    state.version++;
  });
  return state;
}

function safeError(error) {
  const allowed = ['program_call_limit', 'invalid_program_arguments', 'invalid_program_image',
    'program_argument_limit', 'program_output_limit', 'program_cancelled'];
  return allowed.includes(error?.programError) ? error.programError : 'program_execution_failed';
}

function resumeLive(state, input) {
  if (!Array.isArray(input.results) || input.results.length > MAX_NEW_CALLS) fail('invalid_program_results');
  const seen = new Set();
  const prepared = [];
  for (const result of input.results) {
    if (!result || !Number.isInteger(result.sequence) || result.sequence < 0
      || !primordials.hasOwn(result, 'result') || seen.has(result.sequence)) fail('invalid_program_results');
    seen.add(result.sequence);
    const call = state.calls[result.sequence];
    if (!call) fail('program_result_unknown');
    const value = copy(result.result, BYTE_LIMIT, 'program_result_limit');
    if (primordials.hasOwn(call, 'result')) {
      if (canonical(call.result) !== canonical(value)
        || result.resolution_order !== undefined && result.resolution_order !== call.resolution_order) fail('program_result_conflict');
      continue;
    }
    if (!state.pending.has(result.sequence)) fail('program_result_unknown');
    prepared.push({ sequence: result.sequence, value, order: result.resolution_order });
  }
  const ordered = prepared.some(result => result.order !== undefined);
  if (ordered) {
    if (prepared.some(result => !Number.isInteger(result.order))) fail('invalid_program_results');
    prepared.sort((left, right) => left.order - right.order);
    prepared.forEach((result, index) => {
      if (result.order !== state.resolution + index) fail('invalid_program_results');
    });
  }
  // Validate the entire batch before resolving a single live program promise.
  for (const result of prepared) {
    const call = state.calls[result.sequence];
    call.result = result.value;
    call.resolution_order = state.resolution++;
    const waiting = state.pending.get(result.sequence);
    state.pending.delete(result.sequence);
    waiting.resolve(copy(result.value, BYTE_LIMIT, 'program_result_limit'));
    state.version++;
  }
}

async function liveProgress(state) {
  let version = state.version;
  let quiet = 0;
  for (let turn = 0; turn < MAX_MICROTASK_TURNS; turn++) {
    await primordials.Promise.resolve();
    if (state.cancelled || state.error) return { status: 'failed', error: state.cancelled ? 'program_cancelled' : state.error,
      program_id: state.id, calls: [] };
    if (state.finished && !state.pending.size) {
      const result = state.text.length ? state.text.join('\n') : state.result;
      const reply = { status: 'completed', program_id: state.id, result, calls: [] };
      if (state.media.length) reply.media = state.media;
      return reply;
    }
    quiet = version === state.version ? quiet + 1 : 0;
    version = state.version;
    if (quiet >= 16 && state.pending.size) break;
  }
  if (!state.pending.size) {
    state.error = 'program_microtask_limit';
    return { status: 'failed', error: state.error, program_id: state.id, calls: [] };
  }
  const calls = state.calls.filter(call => !primordials.hasOwn(call, 'result'))
    .map(({ sequence, target, arguments: args }) => ({ sequence, target, arguments: args }));
  return { status: 'paused', program_id: state.id, calls };
}

async function live(input) {
  if (!input || typeof input !== 'object' || typeof input.program_id !== 'string'
    || !/^[A-Za-z0-9_-]{1,128}$/.test(input.program_id)) fail('invalid_program_input');
  const operation = input.op ?? 'start';
  if (operation === 'start') {
    if (!activeProgram) activeProgram = liveProgram(input);
    else {
      if (activeProgram.id !== input.program_id) fail('program_conflict');
      const policy = inputState(input);
      const signature = canonical({ tools: input.tools ?? [], max_calls: policy.maximum, max_output_bytes: policy.outputLimit });
      if (signature !== activeProgram.signature) fail('program_conflict');
    }
  } else {
    if (!activeProgram) return { status: 'failed', error: 'runtime_missing', program_id: input.program_id, calls: [] };
    if (activeProgram.id !== input.program_id) fail('program_conflict');
    if (operation === 'resume') resumeLive(activeProgram, input);
    else if (operation === 'cancel') {
      if (activeProgram.finished && !activeProgram.pending.size) return liveProgress(activeProgram);
      activeProgram.cancelled = true;
      for (const waiting of activeProgram.pending.values()) waiting.resolve({ error: { code: 'program_cancelled' } });
      activeProgram.pending.clear();
    } else fail('invalid_program_operation');
  }
  return liveProgress(activeProgram);
}

async function segment(input) {
  const state = inputState(input);
  const tape = entropyTape(input.entropy);
  installEntropy(tape);
  const calls = [];
  const pending = new Map();
  const unknown = [];
  const textParts = [];
  const media = [];
  let resolution = 0;
  let finished = false;
  let finalValue;
  let failed;

  function outputText(value) {
    const next = typeof value === 'string' ? value : json(value, state.outputLimit, 'program_output_limit');
    textParts.push(next);
    json(textParts, state.outputLimit, 'program_output_limit');
  }

  function outputImage(value) {
    if (typeof value !== 'string' || !/^data:image\/(png|jpeg|webp);base64,(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/.test(value)
      || value.endsWith(',')) fail('invalid_program_image');
    media.push({ type: 'image', image_url: value });
    json(media, state.outputLimit, 'program_output_limit');
  }

  Object.defineProperties(globalThis, {
    text: { value: outputText, writable: false, configurable: false },
    image: { value: outputImage, writable: false, configurable: false },
  });

  function invoke(target, kind, argumentsValue) {
    if (calls.length >= state.maximum) fail('program_call_limit');
    if (kind === 'function' && (argumentsValue === null || typeof argumentsValue !== 'object' || Array.isArray(argumentsValue))
      || kind === 'custom' && typeof argumentsValue !== 'string') fail('invalid_program_arguments');
    const args = copy(argumentsValue, BYTE_LIMIT, 'program_argument_limit');
    const sequence = calls.length;
    const prior = state.known[sequence];
    if (prior && (canonical(prior.target) !== canonical(target) || canonical(prior.arguments) !== canonical(args))) fail('program_replay_conflict');
    const call = { sequence, target: target.slice(), arguments: args };
    calls.push(call);
    if (!prior || !primordials.hasOwn(prior, 'result')) {
      if (unknown.length >= MAX_NEW_CALLS) fail('program_call_limit');
      unknown.push(call);
    }
    return new primordials.Promise(resolve => { pending.set(sequence, { resolve, settled: false }); });
  }

  const tools = toolTree(input.tools ?? [], invoke);
  primordials.Promise.resolve().then(() => program(tools)).then(value => {
    try { finalValue = value === undefined ? undefined : copy(value, state.outputLimit, 'program_output_limit'); }
    catch (error) { failed = error; }
    finished = true;
  }, error => { failed = error; finished = true; });

  let quiet = 0;
  for (let turn = 0; turn < MAX_MICROTASK_TURNS; turn++) {
    await primordials.Promise.resolve();
    if (failed) {
      const code = failed?.programError;
      const allowed = ['program_call_limit', 'invalid_program_arguments', 'invalid_program_image', 'program_argument_limit',
        'program_output_limit', 'program_replay_conflict', 'program_entropy_limit'];
      return { status: 'failed', error: allowed.includes(code) ? code : 'program_execution_failed', calls: [], entropy: tape.values };
    }
    const record = state.resolved[resolution];
    const waiting = record && pending.get(record.sequence);
    if (waiting && !waiting.settled) {
      waiting.settled = true;
      waiting.resolve(copy(record.result, BYTE_LIMIT, 'program_result_limit'));
      resolution++;
      quiet = 0;
      // One event-loop checkpoint per recorded settlement preserves races.
      continue;
    }
    quiet++;
    if (quiet < 16) continue;
    if (calls.length < state.known.length || resolution < state.resolved.length) fail('program_replay_conflict');
    tape.complete();
    if (unknown.length) return { status: 'paused', calls: unknown, entropy: tape.values };
    if (finished) {
      const result = textParts.length ? textParts.join('\n')
        : finalValue === undefined ? '' : typeof finalValue === 'string' ? finalValue
          : primordials.stringify(finalValue);
      const reply = { status: 'completed', result, calls: [], entropy: tape.values };
      if (media.length) reply.media = media;
      return reply;
    }
    return { status: 'failed', error: 'program_did_not_complete', calls: [], entropy: tape.values };
  }
  return { status: 'failed', error: 'program_microtask_limit', calls: [], entropy: tape.values };
}

function failure(error, programId) {
  const allowed = ['invalid_program_input', 'program_input_limit', 'invalid_program_limits',
    'invalid_program_journal', 'invalid_program_tools', 'invalid_program_entropy',
    'program_entropy_limit', 'program_replay_conflict', 'program_output_limit', 'program_argument_limit',
    'program_result_limit', 'invalid_live_program_input', 'program_conflict',
    'invalid_program_results', 'program_result_unknown', 'program_result_conflict',
    'invalid_program_operation', 'unsupported_runtime_profile', 'invalid_program_frame',
    'program_frame_limit', 'program_channel_closed', 'program_already_started'];
  const reply = { status: 'failed', error: allowed.includes(error?.programError)
    ? error.programError : 'program_execution_failed', calls: [] };
  if (typeof programId === 'string' && /^[A-Za-z0-9_-]{1,128}$/.test(programId)) reply.program_id = programId;
  return reply;
}

function revokeProgram(state) {
  if (!state) return;
  state.cancelled = true;
  for (const waiting of state.pending.values()) waiting.resolve({ error: { code: 'program_cancelled' } });
  state.pending.clear();
}

function ndjsonFrames(body) {
  if (!body) fail('invalid_program_frame');
  const reader = body.getReader();
  let chunk = new Uint8Array();
  let offset = 0;
  let parts = [];
  let bytes = 0;
  let count = 0;
  let ended = false;
  return {
    async next() {
      if (ended) return null;
      for (;;) {
        if (offset === chunk.length) {
          const next = await reader.read();
          if (next.done) {
            ended = true;
            if (bytes) fail('invalid_program_frame');
            return null;
          }
          if (!ArrayBuffer.isView(next.value) || next.value.BYTES_PER_ELEMENT !== 1) fail('invalid_program_frame');
          chunk = new Uint8Array(next.value.buffer, next.value.byteOffset, next.value.byteLength);
          offset = 0;
        }
        const newline = chunk.indexOf(10, offset);
        const end = newline < 0 ? chunk.length : newline;
        const length = end - offset;
        // Bound the unfinished frame before allocating a joined frame or text.
        if (bytes + length > BYTE_LIMIT) fail('program_frame_limit');
        if (length) parts.push(chunk.subarray(offset, end));
        bytes += length;
        offset = newline < 0 ? end : end + 1;
        if (newline < 0) continue;
        if (++count > MAX_CHANNEL_FRAMES) fail('program_frame_limit');
        const frame = new Uint8Array(bytes);
        let position = 0;
        for (const part of parts) { frame.set(part, position); position += part.length; }
        parts = [];
        bytes = 0;
        try {
          return primordials.parse(new primordials.TextDecoder('utf-8', { fatal: true }).decode(frame));
        } catch { fail('invalid_program_frame'); }
      }
    },
    async cancel() {
      ended = true;
      parts = [];
      bytes = 0;
      try { await reader.cancel(); } catch {}
    },
  };
}

function ndjsonResponse(stream) {
  return new primordials.Response(stream, { headers: {
    'Content-Type': 'application/x-ndjson', 'Cache-Control': 'no-store',
  } });
}

async function duplex(request) {
  let frames;
  let first;
  try {
    frames = ndjsonFrames(request.body);
    first = await frames.next();
    if (!first) fail('program_channel_closed');
    if (first.op !== 'start') fail('invalid_program_operation');
    if (duplexStarted) fail('program_already_started');
    duplexStarted = true;
  } catch (error) {
    if (frames) await frames.cancel();
    return ndjsonResponse(`${primordials.stringify(failure(error, first?.program_id))}\n`);
  }
  let closed = false;
  // Keep real native input I/O pending while the original async program waits.
  // A parked response stream alone does not keep its Worker I/O context alive.
  const readAhead = () => frames.next().then(value => ({ value }), error => ({ error }));
  let nextFrame = readAhead();
  const stream = new primordials.ReadableStream({
    async start(controller) {
      const emit = reply => controller.enqueue(new primordials.TextEncoder().encode(
        `${json(reply, BYTE_LIMIT, 'program_output_limit')}\n`));
      let command = first;
      try {
        for (;;) {
          const reply = await live(command);
          if (closed) return;
          emit(reply);
          if (reply.status !== 'paused') break;
          const received = await nextFrame;
          if (closed) return;
          if (received.error) throw received.error;
          if (!received.value) fail('program_channel_closed');
          command = received.value;
          if (!['resume', 'cancel'].includes(command.op) || command.program_id !== first.program_id) fail('invalid_program_operation');
          // Start the next native read before releasing any awaited tool result.
          nextFrame = readAhead();
        }
      } catch (error) {
        revokeProgram(activeProgram);
        if (!closed) emit(failure(error, first.program_id));
      } finally {
        if (!closed) {
          closed = true;
          controller.close();
        }
        await frames.cancel();
      }
    },
    async cancel() {
      closed = true;
      revokeProgram(activeProgram);
      await frames.cancel();
    },
  });
  return ndjsonResponse(stream);
}

export default {
  async fetch(request) {
    if (request.headers.get('Content-Type')?.split(';', 1)[0].trim().toLowerCase() === 'application/x-ndjson') {
      return duplex(request);
    }
    let result;
    try {
      const text = await request.text();
      if (new primordials.TextEncoder().encode(text).length > BYTE_LIMIT) fail('program_input_limit');
      const input = primordials.parse(text);
      if (input.profile === 'replay') {
        if (!ENABLE_REPLAY_PROFILE) fail('unsupported_runtime_profile');
        result = await segment(input);
      } else if (input.profile === 'json-live' && ENABLE_JSON_LIVE_PROFILE) result = await live(input);
      else fail('unsupported_runtime_profile');
      json(result, BYTE_LIMIT, 'program_output_limit');
    } catch (error) {
      result = failure(error);
    }
    return new primordials.Response(primordials.stringify(result), {
      headers: { 'Content-Type': 'application/json', 'Cache-Control': 'no-store' },
    });
  },
};
