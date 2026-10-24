import { createParser } from "eventsource-parser";
import { ApiError, responseFor } from "./api";

export type ResponseSummary = {
  id: string;
  status: string;
  output?: {
    type: string;
    call_id?: string;
    name?: string;
    arguments?: string;
  }[];
};

export async function streamResponse(
  init: RequestInit,
  onDelta: (text: string) => void,
  onResponse?: (value: ResponseSummary) => void,
): Promise<ResponseSummary> {
  const response = await responseFor("/v1/responses", init);
  if (
    !response.headers.get("content-type")?.includes("text/event-stream") ||
    !response.body
  )
    throw new Error("Invalid response stream");
  let completed: ResponseSummary | undefined;
  const parser = createParser({
    onEvent: (event) => {
      if (event.data === "[DONE]") return;
      const value = JSON.parse(event.data);
      if (
        value.type === "response.output_text.delta" &&
        typeof value.delta === "string"
      )
        onDelta(value.delta);
      if (
        value.type === "response.created" ||
        value.type === "response.in_progress"
      )
        onResponse?.(value.response);
      if (value.type === "response.completed") {
        if (value.response?.status !== "completed")
          throw new Error("Response incomplete");
        completed = value.response;
        onResponse?.(value.response);
      }
      if (
        [
          "response.failed",
          "response.incomplete",
          "response.cancelled",
          "error",
        ].includes(value.type)
      ) {
        const error = value.response?.error ?? value.error;
        throw new ApiError(
          error?.message ?? value.message ?? "Response incomplete",
          502,
          undefined,
          error?.code ??
            value.response?.incomplete_details?.reason ??
            value.code,
        );
      }
    },
  });
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      parser.feed(decoder.decode(value, { stream: true }));
    }
    parser.feed(decoder.decode());
    if (!completed) throw new Error("Response stream interrupted");
    return completed;
  } finally {
    await reader.cancel().catch(() => undefined);
    reader.releaseLock();
  }
}
