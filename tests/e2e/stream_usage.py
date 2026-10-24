"""A complete agent stream must end normally and report authoritative usage."""
from collections.abc import Iterable

from openai.types import CompletionUsage
from openai.types.chat import ChatCompletionChunk


def reported_stream_usage(chunks: Iterable[ChatCompletionChunk]) -> CompletionUsage:
    finished = False
    usage: CompletionUsage | None = None
    for chunk in chunks:
        for choice in chunk.choices:
            if choice.finish_reason is not None:
                assert choice.finish_reason == "stop", "agent stream did not finish normally"
                finished = True
        if chunk.usage is not None:
            usage = chunk.usage
    assert finished, "agent stream ended before a terminal stop"
    assert usage is not None, "agent stream did not report usage"
    return usage
