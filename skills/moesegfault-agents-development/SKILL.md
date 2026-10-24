---
name: moesegfault-agents-development
description: Change, test, or release the moeSegFault Agents platform repository, including its Rust Workers, Durable Objects, workspace daemon, magents CLI, and React dashboard. Use for platform implementation and infrastructure work, not calling an existing agent through the CLI or SDK.
---

# moeSegFault Agents platform development

Work on the platform's implementation without changing the caller's task into a production operation. Locate the intended checkout by its `Cargo.toml`, `wrangler.jsonc`, and `crates/` tree. Inspect its current branch and worktree before editing; preserve unrelated work and use an isolated worktree when another task owns the checkout.

## Choose the relevant boundary

| Work | Read when needed |
| --- | --- |
| Harness, tools, provider adapters, persistence, billing, or recovery | [Runtime ownership](references/runtime.md) |
| Locate components and select useful checks | [Changes and verification](references/verification.md) |
| Provision, migrate, deploy, package, or diagnose a release | [Delivery](references/delivery.md) |

Do not load all references for an ordinary targeted change. Read the relevant repository document and implementation next; the checked-out code and workflow are authoritative when they have evolved beyond these guides.

## Keep the external contract intact

- Preserve existing HTTP/CLI behavior and readable stored data. Agent or registry edits affect new sessions, not an existing session's pinned prefix.
- Separate authoritative DO conversation state, D1 admission/accounting, D1 status projections, and the R2 checkpoint manifest. A lagging projection is not proof that execution failed.
- Journal uncertain external operations; never rerun Bash, replay a provider call, release an unknown hold, or reset data merely to obtain a green test.
- Keep keys and user content out of telemetry and routine build output. Browser code must not own provider credentials or account bearer tokens.

Prefer the existing Rust/platform abstractions and mature libraries over a parallel harness or authentication system. Verify the affected user journey as well as local invariants. Repository-development instructions do not themselves authorize live model calls, migrations, secret changes, or production delivery.
