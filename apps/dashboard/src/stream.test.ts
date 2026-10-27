import { afterEach, describe, expect, it, vi } from "vitest";
import { streamCompletion } from "./api";
afterEach(() => vi.unstubAllGlobals());
describe("progressive completion stream", () => {
  it("delivers fragmented Unicode deltas before stream completion", async () => {
    let controller!: ReadableStreamDefaultController<Uint8Array>;
    const body = new ReadableStream<Uint8Array>({
      start(value) {
        controller = value;
      },
    });
    vi.stubGlobal(
      "fetch",
      vi.fn(
        async () =>
          new Response(body, {
            headers: { "content-type": "text/event-stream" },
          }),
      ),
    );
    const received: string[] = [];
    const pending = streamCompletion(
      "/v1/sessions/s/messages",
      { method: "POST" },
      (delta) => received.push(delta),
    );
    const wire = new TextEncoder().encode(
      'data: {"choices":[{"delta":{"content":"你好"}}]}\r\n\r\n',
    );
    for (const byte of wire) controller.enqueue(Uint8Array.of(byte));
    await vi.waitFor(() => expect(received).toEqual(["你好"]));
    controller.enqueue(
      new TextEncoder().encode(
        'data: {"choices":[{"delta":{},"finish_reason":"stop"}]}\n\ndata: [DONE]\n\n',
      ),
    );
    controller.close();
    await pending;
  });
  it("rejects truncated streams instead of reporting success", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(
        async () =>
          new Response(
            'data: {"choices":[{"delta":{"content":"partial"}}]}\n\n',
            { headers: { "content-type": "text/event-stream" } },
          ),
      ),
    );
    await expect(streamCompletion("/stream", {}, () => {})).rejects.toThrow(
      "Response stream interrupted",
    );
  });
  it("does not accept DONE without a successful terminal chunk", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(
        async () =>
          new Response("data: [DONE]\n\n", {
            headers: { "content-type": "text/event-stream" },
          }),
      ),
    );
    await expect(streamCompletion("/stream", {}, () => {})).rejects.toThrow(
      "Response stream interrupted",
    );
  });
  it("surfaces structured SSE provider errors", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(
        async () =>
          new Response(
            'data: {"error":{"code":"provider_error","message":"Unavailable"}}\n\n',
            { headers: { "content-type": "text/event-stream" } },
          ),
      ),
    );
    await expect(
      streamCompletion("/stream", {}, () => {}),
    ).rejects.toMatchObject({ code: "provider_error", status: 502 });
  });
});
