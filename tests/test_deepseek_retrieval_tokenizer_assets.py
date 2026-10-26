"""Verify the new shared plain vocabulary without rerunning prompt-count suites."""
from __future__ import annotations
import gzip
import hashlib
import json
from pathlib import Path
import unittest
from typing import Any

ROOT: Path = Path(__file__).resolve().parents[1]
ASSETS: Path = ROOT / 'third_party/deepseek-recipe/assets'


class SharedPlainVocabulary(unittest.TestCase):
    def test_v4_and_v41_plain_configuration_is_identical(self) -> None:
        expected: str = 'b64f58e1ec67577c2fc4b6b93689b12da288a974e46c088bad52ae6e5defe5f8'
        manifest: dict[str, Any] = json.loads((ASSETS / 'manifest.json').read_text())
        for profile in ['v4', 'v41']:
            value: dict[str, Any] = json.loads(gzip.decompress((ASSETS / f'{profile}.json.gz').read_bytes()))
            value['added_tokens'] = []
            encoded: bytes = json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(',', ':')).encode()
            self.assertEqual(hashlib.sha256(encoded).hexdigest(), expected)
            self.assertEqual(manifest[profile]['plain_vocabulary_sha256'], expected)
        source: str = (ROOT / 'crates/agents-worker/src/deepseek_tokenizer.rs').read_text()
        self.assertIn(expected, source)

    def test_narrow_plain_accessors_have_lf_source_provenance(self) -> None:
        root: Path = ASSETS.parent
        path: str = 'deepseek-recipe-encoding/src/tokenizer.rs'
        data: bytes = (root / path).read_bytes()
        manifest: dict[str, Any] = json.loads((root / 'source-manifest.json').read_text())
        self.assertNotIn(b'\r\n', data)
        self.assertEqual(hashlib.sha256(data).hexdigest(), manifest['files'][path]['vendored_sha256'])
        self.assertIn(b'pub fn encode_plain_text', data)
        self.assertIn(b'pub fn count_plain_text', data)


if __name__ == '__main__':
    unittest.main()
