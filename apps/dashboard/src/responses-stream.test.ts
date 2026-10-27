import { afterEach, describe, expect, it, vi } from "vitest";
import { streamResponse } from "./responses-stream";

afterEach(() => vi.unstubAllGlobals());
function stream(events: unknown[]) {
  vi.stubGlobal(
    "fetch",
    vi.fn(
      async () =>
        new Response(
          events.map((event) => `data: ${JSON.stringify(event)}\n\n`).join(""),
          { headers: { "Content-Type": "text/event-stream" } },
        ),
    ),
  );
}

describe("Responses streaming", () => {
  it("handles the standard error event used to end a cancelled response", async () => {
    stream([
      {
        type: "error",
        code: "response_cancelled",
        message: "Response cancelled",
        sequence_number: 4,
      },
    ]);
    await expect(streamResponse({}, () => {})).rejects.toMatchObject({
      code: "response_cancelled",
      message: "Response cancelled",
    });
  });
  it("delivers text deltas and requires the genuine completed response", async () => {
    const result = {
      id: "resp-1",
      status: "completed",
      output: [{ type: "message" }],
    };
    stream([
      {
        type: "response.created",
        response: { id: "resp-1", status: "in_progress" },
      },
      { type: "response.output_text.delta", delta: "你好" },
      { type: "response.completed", response: result },
    ]);
    const received: string[] = [];
    expect(await streamResponse({}, (text) => received.push(text))).toEqual(
      result,
    );
    expect(received).toEqual(["你好"]);
  });
  it("treats completed function calls as a client handoff, not text or local execution", async () => {
    const result = {
      id: "resp-local",
      status: "completed",
      output: [
        {
          type: "function_call",
          call_id: "call-1",
          name: "local_search",
          arguments: '{"q":"example"}',
        },
      ],
    };
    stream([
      { type: "response.function_call_arguments.delta", delta: '{"q":' },
      { type: "response.completed", response: result },
    ]);
    const text = vi.fn();
    expect(await streamResponse({}, text)).toEqual(result);
    expect(text).not.toHaveBeenCalled();
  });
  it.each(["response.failed", "response.incomplete", "response.cancelled"])(
    "surfaces %s rather than treating EOF as success",
    async (type) => {
      stream([
        {
          type,
          response: {
            id: "resp-1",
            status: type.slice(9),
            error: { code: "bounded-failure", message: "Stopped" },
          },
        },
      ]);
      await expect(streamResponse({}, () => {})).rejects.toMatchObject({
        code: "bounded-failure",
      });
    },
  );
  it("rejects a stream ending after partial text", async () => {
    stream([{ type: "response.output_text.delta", delta: "partial" }]);
    await expect(streamResponse({}, () => {})).rejects.toThrow(
      "Response stream interrupted",
    );
  });
});
