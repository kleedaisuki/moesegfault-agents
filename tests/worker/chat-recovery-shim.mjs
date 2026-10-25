import { WorkerEntrypoint } from 'cloudflare:workers';
import RustWorker from '../index.js';
export * from '../index.js';

// This module replaces only the Miniflare test entrypoint. The distributable
// Worker never imports it or recognizes this fixture header/control service.
const controlOrigin = 'https://chat-recovery-control.test';
const inactive = 'Connection closed: this Durable Object instance is no longer active. Reconnect or retry the request.';

async function control(caseId, operation, fields = {}) {
  const response = await fetch(controlOrigin, {
    method: 'POST', headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ caseId, operation, ...fields }),
  });
  if (!response.ok) throw new Error('Invalid Chat recovery test control');
  return response.json();
}

async function cutBody(response, caseId, mode) {
  const reader = response.body.getReader();
  if (['before', 'overload', 'unknown', 'abort'].includes(mode)) {
    await control(caseId, 'cut');
    await reader.cancel().catch(() => {});
    return new ReadableStream({ start(controller) { controller.close(); } });
  }
  const decoder = new TextDecoder();
  const encoder = new TextEncoder();
  let buffer = '';
  let closeNext = false;
  let errorNext = false;
  return new ReadableStream({
    async pull(controller) {
      if (errorNext) { controller.error(new Error(inactive)); return; }
      if (closeNext) { controller.close(); return; }
      try {
        while (true) {
          const end = buffer.indexOf('\n\n');
          if (end >= 0) {
            const block = buffer.slice(0, end);
            buffer = buffer.slice(end + 2);
            controller.enqueue(encoder.encode(block + '\n\n'));
            const line = block.split('\n').find(value => value.startsWith('data: '));
            let value;
            if (line && line.slice(6) !== '[DONE]') value = JSON.parse(line.slice(6));
            const choice = value?.choices?.[0];
            const prefix = typeof choice?.delta?.content === 'string' && choice.delta.content.length > 0;
            if ((mode === 'terminal' && choice?.finish_reason === 'stop')
              || (['prefix-error', 'mismatch'].includes(mode) && prefix)) {
              await control(caseId, 'cut');
              await reader.cancel().catch(() => {});
              errorNext = mode === 'prefix-error';
              closeNext = !errorNext;
            }
            return;
          }
          const item = await reader.read();
          if (item.done) { controller.close(); return; }
          buffer += decoder.decode(item.value, { stream: true });
        }
      } catch (error) { controller.error(error); }
    },
    cancel(reason) { return reader.cancel(reason).catch(() => {}); },
  });
}

function namespace(real, caseId) {
  return new Proxy(real, {
    get(target, property) {
      // worker-rs checks constructor.name when resolving an Env binding.
      if (property === 'constructor') return target.constructor;
      if (property === 'get') return (id, options) => {
        const stub = target.get(id, options);
        return new Proxy(stub, {
          get(instance, name) {
            if (name === 'constructor') return instance.constructor;
            if (name !== 'fetch') {
              const value = Reflect.get(instance, name, instance);
              return typeof value === 'function' ? value.bind(instance) : value;
            }
            return async (...arguments_) => {
              const request = new Request(...arguments_);
              const path = new URL(request.url).pathname;
              if (path.startsWith('/chat-runs/')) {
                const plan = await control(caseId, 'read', { runId: path.split('/').at(-1) });
                if (plan.mode === 'overload') {
                  const error = new Error('Synthetic Durable Object overload');
                  error.overloaded = true;
                  throw error;
                }
                const response = await instance.fetch(request);
                await control(caseId, 'read-result', { status: response.status });
                if (plan.mode !== 'mismatch' || response.status !== 200) return response;
                const value = await response.json();
                value.text = 'WRONG_PREFIX_' + value.text;
                return Response.json(value, { status: response.status, headers: response.headers });
              }
              const response = await instance.fetch(request);
              if (path !== '/run' || request.method !== 'POST'
                || !response.headers.get('content-type')?.includes('text/event-stream')) return response;
              const plan = await control(caseId, 'open', { runId: response.headers.get('x-agents-run') });
              if (plan.mode === 'pass') return response;
              const body = await cutBody(response, caseId, plan.mode);
              return new Response(body, { status: response.status, headers: response.headers });
            };
          },
        });
      };
      const value = Reflect.get(target, property, target);
      return typeof value === 'function' ? value.bind(target) : value;
    },
  });
}

export default class extends WorkerEntrypoint {
  fetch(request) {
    const caseId = request.headers.get('x-worker-test-chat-recovery');
    const env = caseId ? new Proxy(this.env, {
      get(target, name) { return name === 'SESSIONS' ? namespace(target.SESSIONS, caseId) : Reflect.get(target, name, target); },
    }) : this.env;
    return RustWorker.prototype.fetch.call({ env, ctx: this.ctx }, request);
  }
  scheduled(event) { return RustWorker.prototype.scheduled.call({ env: this.env, ctx: this.ctx }, event); }
}
