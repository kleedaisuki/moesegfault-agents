"""Verify pinned managed tokenizer assets and the narrowly patched dependency."""
from __future__ import annotations
import gzip
import hashlib
import json
from pathlib import Path
import unittest
from typing import Any

ROOT: Path = Path(__file__).resolve().parents[1]
VENDOR: Path = ROOT / 'third_party/deepseek-recipe'


class TokenizerAssets(unittest.TestCase):
    def test_pinned_compressed_assets(self) -> None:
        manifest: dict[str, Any] = json.loads((VENDOR / 'assets/manifest.json').read_text())
        self.assertEqual(set(manifest), {'v4', 'v41'})
        for profile, expected in manifest.items():
            compressed: bytes = (VENDOR / f'assets/{profile}.json.gz').read_bytes()
            data: bytes = gzip.decompress(compressed)
            self.assertEqual(len(data), expected['bytes'])
            self.assertEqual(len(compressed), expected['gzip_bytes'])
            self.assertEqual(hashlib.sha256(data).hexdigest(), expected['sha256'])
            self.assertEqual(hashlib.sha256(compressed).hexdigest(), expected['gzip_sha256'])
            self.assertLess(len(data), 6_367_249)
            self.assertEqual(compressed[4:8], b'\0\0\0\0')

    def test_only_three_explicit_recipe_source_patches(self) -> None:
        manifest: dict[str, Any] = json.loads((VENDOR / 'source-manifest.json').read_text())
        patched: set[str] = set()
        for name, hashes in manifest['files'].items():
            self.assertEqual(hashlib.sha256((VENDOR / name).read_bytes()).hexdigest(), hashes['vendored_sha256'], name)
            if hashes['vendored_sha256'] != hashes['upstream_sha256']:
                patched.add(name)
        self.assertEqual(patched, {'deepseek-recipe/src/lib.rs', 'deepseek-recipe/src/protocol/json_schema.rs', 'deepseek-recipe-encoding/src/tokenizer.rs'})
        self.assertEqual(manifest['recipe_revision'], '935817c66f5d6e64838f419692545f06787e485f')
        for notice in ['LICENSE', 'serde-json/LICENSE-MIT', 'serde-json/LICENSE-APACHE', 'assets/LICENSE']:
            self.assertTrue((VENDOR / notice).is_file())

    def test_reference_corpus_is_synthetic_and_offline(self) -> None:
        fixtures: dict[str, Any] = json.loads((ROOT / 'tests/fixtures/deepseek-token-count.json').read_text(encoding='utf-8'))
        self.assertEqual(fixtures['api_calls'], 0)
        counts: dict[str, int] = {case['name']: case['input_tokens'] for case in fixtures['fixtures']}
        self.assertEqual(counts, {'plain': 6, 'thinking-high': 32, 'chinese': 13, 'function-schema': 253, 'user-control-spelling': 17, 'filler': 2053})

    def test_ordered_json_is_isolated_from_platform_package(self) -> None:
        manifest: str = (VENDOR / 'Cargo.toml').read_text()
        self.assertIn('package = "deepseek-serde-json"', manifest)
        self.assertIn('default-features = false, features = ["unstable_wasm"]', manifest)
        package: str = (VENDOR / 'serde-json/Cargo.toml').read_text()
        self.assertIn('name = "deepseek-serde-json"', package)


if __name__ == '__main__':
    unittest.main()
