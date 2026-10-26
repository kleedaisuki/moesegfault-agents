"""Offline synthetic vision corpus from the pinned official 1017 renderer."""
from __future__ import annotations
import argparse
import base64
import hashlib
import importlib.util
import json
from pathlib import Path
import struct
from types import ModuleType
from typing import Any
import zlib
from tokenizers import Tokenizer


def small_image(format_name: str) -> bytes:
    """Pinned synthetic RGB pixel (Pillow 12.0 default JPEG/WebP encoders)."""
    assets: dict[str, str] = {"JPEG":"/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDAAgGBgcGBQgHBwcJCQgKDBQNDAsLDBkSEw8UHRofHh0aHBwgJC4nICIsIxwcKDcpLDAxNDQ0Hyc5PTgyPC4zNDL/2wBDAQkJCQwLDBgNDRgyIRwhMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjL/wAARCAABAAEDASIAAhEBAxEB/8QAHwAAAQUBAQEBAQEAAAAAAAAAAAECAwQFBgcICQoL/8QAtRAAAgEDAwIEAwUFBAQAAAF9AQIDAAQRBRIhMUEGE1FhByJxFDKBkaEII0KxwRVS0fAkM2JyggkKFhcYGRolJicoKSo0NTY3ODk6Q0RFRkdISUpTVFVWV1hZWmNkZWZnaGlqc3R1dnd4eXqDhIWGh4iJipKTlJWWl5iZmqKjpKWmp6ipqrKztLW2t7i5usLDxMXGx8jJytLT1NXW19jZ2uHi4+Tl5ufo6erx8vP09fb3+Pn6/8QAHwEAAwEBAQEBAQEBAQAAAAAAAAECAwQFBgcICQoL/8QAtREAAgECBAQDBAcFBAQAAQJ3AAECAxEEBSExBhJBUQdhcRMiMoEIFEKRobHBCSMzUvAVYnLRChYkNOEl8RcYGRomJygpKjU2Nzg5OkNERUZHSElKU1RVVldYWVpjZGVmZ2hpanN0dXZ3eHl6goOEhYaHiImKkpOUlZaXmJmaoqOkpaanqKmqsrO0tba3uLm6wsPExcbHyMnK0tPU1dbX2Nna4uPk5ebn6Onq8vP09fb3+Pn6/9oADAMBAAIRAxEAPwBKKKKAP//Z","WEBP":"UklGRiQAAABXRUJQVlA4IBgAAABQAQCdASoBAAEAAUAmJaQABHQAAP4AAAA="}
    return base64.b64decode(assets[format_name])


def png(width: int, height: int) -> bytes:
    """Serialize solid synthetic fixture pixels; not a production image codec."""
    def chunk(kind: bytes, data: bytes) -> bytes:
        return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data))
    row: bytes = b'\0' + b'\x7f\x7f\x7f' * width
    compressor: Any = zlib.compressobj()
    compressed: bytes = b''.join(compressor.compress(row) for _ in range(height)) + compressor.flush()
    return b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', width, height, 8, 2, 0, 0, 0)) + chunk(b'IDAT', compressed) + chunk(b'IEND', b'')


def main() -> None:
    parser: argparse.ArgumentParser = argparse.ArgumentParser()
    parser.add_argument('--encoding', type=Path, required=True)
    parser.add_argument('--tokenizer', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args: argparse.Namespace = parser.parse_args()
    encoding: bytes = args.encoding.read_bytes()
    assert hashlib.sha256(encoding).hexdigest() == '13231c592e958fb39e2af6001dae69d90e7a36b265702b9b390236fb387599e2'
    assert hashlib.sha256(args.tokenizer.read_bytes()).hexdigest() == 'c90dfa01249db1be4245780a052ede752e1361c612ac6d08e2bdada7d599476b'
    spec: Any = importlib.util.spec_from_file_location('official_image_encoding', args.encoding)
    assert spec is not None and spec.loader is not None
    module: ModuleType = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    tokenizer: Tokenizer = Tokenizer.from_file(str(args.tokenizer))
    assets: dict[str, dict[str, Any]] = {}
    for name, mime, data, width, height in [
        ('small_png', 'image/png', png(1, 1), 1, 1),
        ('square_png', 'image/png', png(1024, 1024), 1024, 1024),
        ('large_png', 'image/png', png(8192, 8192), 8192, 8192),
        ('small_jpeg', 'image/jpeg', small_image('JPEG'), 1, 1),
        ('small_webp', 'image/webp', small_image('WEBP'), 1, 1),
        ('small_gif', 'image/gif', base64.b64decode('R0lGODlhAQABAIAAAAAAAP///ywAAAAAAQABAAACAUwAOw=='), 1, 1),
    ]:
        assets[name] = {'mime': mime, 'base64': base64.b64encode(data).decode(), 'sha256': hashlib.sha256(data).hexdigest(), 'width': width, 'height': height}
    prefix: str = 'You are an agent running in moeSegFault Agents. Tools and skills below are pinned for this session.\nCounter vision fixed instructions.'
    fixtures: list[dict[str, Any]] = []
    cases: list[tuple[str, str, str, str, int]] = [
        ('user_png_original', 'user', 'small_png', 'original', 184),
        ('user_gif_auto', 'user', 'small_gif', 'auto', 184),
        ('developer_png_original', 'developer', 'small_png', 'original', 184),
        ('system_png_original', 'system', 'small_png', 'original', 184),
        ('assistant_png_original', 'assistant', 'small_png', 'original', 184),
        ('user_jpeg_original', 'user', 'small_jpeg', 'original', 184),
        ('user_webp_auto', 'user', 'small_webp', 'auto', 184),
        ('user_png_low', 'user', 'square_png', 'low', 184),
        ('user_png_high', 'user', 'square_png', 'high', 652),
        ('user_png_large_header_only', 'user', 'large_png', 'original', 994),
        ('function_output_png', 'function', 'small_png', 'original', 184),
        ('custom_output_png', 'custom', 'small_png', 'original', 184),
    ]
    for name, role, asset_name, detail, image_tokens in cases:
        asset: dict[str, Any] = assets[asset_name]
        url: str = f"data:{asset['mime']};base64,{asset['base64']}"
        image: dict[str, Any] = {'type': 'input_image', 'image_url': url, 'detail': detail}
        if name == 'user_png_original':
            image['file_id'] = None
        if name == 'user_gif_auto':
            image['detail'] = None
        reference_image: dict[str, Any] = {'type': 'image_url', 'image_url': {'url': url, 'detail': detail}}
        native: list[dict[str, Any]]
        messages: list[dict[str, Any]]
        if role in {'user', 'developer', 'system', 'assistant'}:
            native = [{'role': role, 'content': [{'type': 'input_text', 'text': 'Hello!'}, image]}]
            # Official Responses converter maps developer to V4.1 system.
            messages = [{'role': 'system' if role == 'developer' else role, 'content': [{'type': 'text', 'text': 'Hello!'}, reference_image]}]
        else:
            tool_name: str = 'local_image' if role == 'function' else 'apply_patch'
            arguments: str = '{}' if role == 'function' else json.dumps({'input': 'fixture patch'})
            native = [{'type': 'function_call', 'name': tool_name, 'call_id': 'call_vision', 'arguments': arguments} if role == 'function' else {'type': 'custom_tool_call', 'name': tool_name, 'call_id': 'call_vision', 'input': 'fixture patch'}, {'type': 'function_call_output' if role == 'function' else 'custom_tool_call_output', 'call_id': 'call_vision', 'output': [image]}, {'role': 'user', 'content': 'Continue.'}]
            messages = [{'role': 'assistant', 'content': '', 'tool_calls': [{'id': 'call_vision', 'type': 'function', 'function': {'name': tool_name, 'arguments': arguments}}]}, {'role': 'tool', 'tool_call_id': 'call_vision', 'content': [reference_image]}, {'role': 'user', 'content': 'Continue.'}]
        counts: list[int] = []
        for reference in [messages, [{'role': 'system', 'content': prefix}, *messages]]:
            prompt: str
            images: list[dict[str, Any]]
            prompt, images = module.encode_case({'messages': reference, 'thinking_mode': 'chat'}, 'chat')
            assert len(images) == 1
            counts.append(len(tokenizer.encode(prompt, add_special_tokens=False).ids) + image_tokens - 1)
        fixtures.append({'name': name, 'asset': asset_name, 'image_tokens': image_tokens,
                         'body': {'model': 'deepseek-flash', 'input': native, 'reasoning': {'effort': 'none'}},
                         'input_tokens': counts[0], 'public_input_tokens': counts[1]})
    output: dict[str, Any] = {'renderer_sha256': hashlib.sha256(encoding).hexdigest(), 'recipe_revision': '935817c66f5d6e64838f419692545f06787e485f', 'api_calls': 0, 'assets': assets, 'fixtures': fixtures}
    args.output.write_text(json.dumps(output, ensure_ascii=False, indent=2) + '\n', encoding='utf-8')
    print(json.dumps({item['name']: [item['input_tokens'], item['public_input_tokens']] for item in fixtures}))


if __name__ == '__main__':
    main()
