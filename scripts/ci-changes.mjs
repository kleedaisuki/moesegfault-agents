import { execFileSync } from 'node:child_process';
import { appendFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';

export function affected(files, all = false) {
  const matches = (pattern) => all || files.some((file) => pattern.test(file));
  const global = matches(/^(Cargo\.(toml|lock)|rust-toolchain\.toml|\.github\/workflows\/)/);
  return {
    rust: global || matches(/^crates\//),
    worker: global || matches(/^(crates\/(agents-worker|agents-core|agents-auth)\/|wrangler\.jsonc|package(-lock)?\.json)/),
    dashboard: matches(/^(apps\/dashboard\/|\.github\/workflows\/)/),
    container: global || matches(/^(crates\/workspace-daemon\/|container\/|tests\/e2e\/(test_workspace_daemon\.py|conftest\.py|requirements\.txt)|\.dockerignore)/),
    tooling: matches(/^(scripts\/|package(-lock)?\.json|wrangler\.jsonc|\.github\/)/),
  };
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const base = process.env.CHANGE_BASE;
  const all = process.env.FORCE_ALL === 'true' || !base || /^0+$/.test(base);
  const files = all ? [] : execFileSync('git', ['diff', '--name-only', `${base}...HEAD`], { encoding: 'utf8' }).trim().split('\n');
  const result = affected(files, all);
  for (const [name, value] of Object.entries(result)) {
    const line = `${name}=${value}\n`;
    process.stdout.write(line);
    if (process.env.GITHUB_OUTPUT) appendFileSync(process.env.GITHUB_OUTPUT, line);
  }
}
