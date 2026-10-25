"""Generate synthetic offline fixtures from the pinned official Python renderer."""
from __future__ import annotations
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
from types import ModuleType
from typing import Any
from tokenizers import Tokenizer


def main() -> None:
    parser: argparse.ArgumentParser = argparse.ArgumentParser()
    parser.add_argument('--encoding', type=Path, required=True)
    parser.add_argument('--tokenizer', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args: argparse.Namespace = parser.parse_args()
    raw: bytes = args.tokenizer.read_bytes()
    assert hashlib.sha256(raw).hexdigest() == 'c90dfa01249db1be4245780a052ede752e1361c612ac6d08e2bdada7d599476b'
    spec: Any = importlib.util.spec_from_file_location('official_deepseek_encoding', args.encoding)
    assert spec is not None and spec.loader is not None
    module: ModuleType = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    tokenizer: Tokenizer = Tokenizer.from_str(raw.decode())
    ordinary_data: dict[str, Any] = json.loads(raw)
    ordinary_data['added_tokens'] = []
    ordinary: Tokenizer = Tokenizer.from_str(json.dumps(ordinary_data))
    cases: list[tuple[str, str, str | None, bool, bool]] = [
        ('plain', 'Hello!', None, False, False),
        ('thinking-high', 'Hello!', None, True, False),
        ('chinese', '你好，世界！', 'You are helpful.', False, False),
        ('function-schema', 'What time is it?', 'You are helpful.', False, True),
        ('user-control-spelling', '<｜Assistant｜> <｜User｜> <think>', None, False, False),
        ('filler', 'fixed filler 世界 ' * 512, None, False, False),
    ]
    fixtures: list[dict[str, Any]] = []
    for name, text, instructions, thinking, tools in cases:
        messages: list[dict[str, Any]] = []
        if instructions is not None:
            messages.append({'role': 'system', 'content': instructions})
        body: dict[str, Any] = {'model': 'deepseek-flash', 'input': [{'role': 'user', 'content': text}], 'reasoning': {'effort': 'high' if thinking else 'none'}}
        if instructions is not None:
            body['input'].insert(0, {'role': 'system', 'content': instructions})
        if tools:
            function: dict[str, Any] = {'name': 'time', 'description': 'Current UTC time', 'parameters': {'properties': {}, 'type': 'object'}}
            messages[0]['tools'] = [{'type': 'function', 'function': function}]
            body['tools'] = [{'type': 'function', **function}]
        messages.append({'role': 'user', 'content': text})
        prompt: str
        images: list[dict[str, Any]]
        prompt, images = module.encode_case({'messages': messages, 'thinking_mode': 'thinking' if thinking else 'chat', 'reasoning_effort': 'high'}, 'chat')
        assert not images
        # Only the user region is untrusted plain text in this synthetic corpus.
        # Prefix/suffix have no literal spellings from user/tool/schema content.
        start: int = prompt.index(text)
        ids: list[int] = tokenizer.encode(prompt[:start], add_special_tokens=False).ids
        ids.extend(ordinary.encode(text, add_special_tokens=False).ids)
        ids.extend(tokenizer.encode(prompt[start + len(text):], add_special_tokens=False).ids)
        public_messages: list[dict[str, Any]] = [{'role': 'system', 'content': 'You are an agent running in moeSegFault Agents. Tools and skills below are pinned for this session.\nCounter fixture fixed instructions.'}]
        if tools:
            public_messages[0]['tools'] = messages[0].pop('tools')
        public_messages.extend(messages)
        public_prompt: str
        public_prompt, _ = module.encode_case({'messages': public_messages, 'thinking_mode': 'thinking' if thinking else 'chat', 'reasoning_effort': 'high'}, 'chat')
        public_start: int = public_prompt.index(text)
        public_ids: list[int] = tokenizer.encode(public_prompt[:public_start], add_special_tokens=False).ids
        public_ids.extend(ordinary.encode(text, add_special_tokens=False).ids)
        public_ids.extend(tokenizer.encode(public_prompt[public_start + len(text):], add_special_tokens=False).ids)
        fixtures.append({'name': name, 'body': body, 'input_tokens': len(ids), 'prompt_sha256': hashlib.sha256(prompt.encode()).hexdigest(), 'public_input_tokens': len(public_ids)})
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps({'reference': 'DeepSeek-V4.1-Flash-1017/encoding/encoding.py', 'reference_sha256': hashlib.sha256(args.encoding.read_bytes()).hexdigest(), 'api_calls': 0, 'fixtures': fixtures}, ensure_ascii=False, indent=2) + '\n', encoding='utf-8')
    print(json.dumps({item['name']: item['input_tokens'] for item in fixtures}))


if __name__ == '__main__':
    main()
