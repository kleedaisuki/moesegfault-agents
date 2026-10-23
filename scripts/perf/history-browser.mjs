// Local V8 JSON/structural-sharing cost only; not a browser layout or network benchmark.
import { readFileSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { performance } from 'node:perf_hooks';
import { resolve } from 'node:path';
import { gzipSync } from 'node:zlib';

const require = createRequire(resolve('apps/dashboard/package.json'));
const { replaceEqualDeep } = require('@tanstack/react-query');
function measure(work) {
  for (let i = 0; i < 3; i++) work();
  const values = Array.from({ length: 25 }, () => {
    const start = performance.now();
    work();
    return performance.now() - start;
  }).sort((a, b) => a - b);
  return { samples: values.length, p50_ms: values[12], p95_ms: values[23] };
}
const reports = [4096, 32768].map(bytes => {
  const source = readFileSync(`.cache/perf/history/trace-128-${bytes}.json`, 'utf8');
  const previous = JSON.parse(source);
  return {
    rounds: 128, tool_stdout_bytes: bytes, payload_bytes: Buffer.byteLength(source),
    gzip_bytes: gzipSync(source).length,
    parse: measure(() => JSON.parse(source)),
    unchanged_parse_and_query_structural_sharing: measure(() => replaceEqualDeep(previous, JSON.parse(source))),
    message_elements: previous.messages.length,
  };
});
const report = { node: process.version, platform: process.platform, arch: process.arch, reports,
  note: 'V8 on the local host, not mobile browser paint/layout or an end-to-end deployment measurement.' };
writeFileSync('.cache/perf/history/browser.json', `${JSON.stringify(report, null, 2)}\n`);
process.stdout.write(`${JSON.stringify(report, null, 2)}\n`);
