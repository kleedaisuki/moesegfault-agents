import assert from 'node:assert/strict';
import test from 'node:test';
import { changedRustTests } from './incremental-checks.mjs';

const adjacent = [
  '#[test]', 'fn accepted_before() {', '    assert!(true);', '}', '',
  '#[test]', 'fn new_cache_case() {', '    assert!(true);', '}', '',
  '#[test]', 'fn accepted_after() {', '    assert!(true);', '}',
].join('\n');

test('new Rust test attributes and adjacent whitespace do not replay accepted neighboring bodies', () => {
  assert.deepEqual(changedRustTests(adjacent, [[5, 11]]), ['new_cache_case']);
  assert.deepEqual(changedRustTests(adjacent, [[3, 4]]), ['accepted_before']);
  assert.deepEqual(changedRustTests(adjacent, [[3, 4], [5, 11]]), ['accepted_before', 'new_cache_case']);
  assert.deepEqual(changedRustTests(adjacent, [[13, 13]]), ['accepted_after']);
  assert.deepEqual(changedRustTests(adjacent, [[10, 11]]), []);
});

test('Rust literal and nested-comment braces cannot terminate a selected test scope', () => {
  const source = [
    '#[test]', 'fn with_literals() {', '    let value = r###"', '}',
    '#[test]', 'fn not_a_test() {}', '"###;',
    '    let byte = b\'}\'; let escaped = "\\\" }";',
    '    /* outer } /* nested { */ still } */', '    // }',
    '    assert!(value.contains("}"));', '}', '',
    '#[test]', 'fn new_cache_case() {', '    assert!(true);', '}',
  ].join('\n');
  assert.deepEqual(changedRustTests(source, [[11, 12]]), ['with_literals']);
  assert.deepEqual(changedRustTests(source, [[13, 18]]), ['new_cache_case']);
});

test('Rust lifetime syntax and property-test regexes retain scoped incremental selection', () => {
  const source = [
    'proptest! {', '    #[test]', '    fn arbitrary_input(value in "[{}]{0,10}") {',
    '        fn borrow<\'a>(value: &\'a str) -> &\'a str { value }',
    '        prop_assert_eq!(borrow(&value), &value);', '    }', '}',
    '#[test]', '#[ignore]', 'fn ignored_probe() {}',
  ].join('\n');
  assert.deepEqual(changedRustTests(source, [[2, 7], [8, 11]]), ['arbitrary_input']);
});
