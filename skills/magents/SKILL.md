---
name: magents
description: Operate moeSegFault Agents through the magents CLI, including account access, private agent publication, BYOK, Responses and local function handoff, durable sessions, Space files, workspace versions, and downloads. Use for CLI user workflows, not direct HTTP/SDK integrations or implementation and deployment of the platform itself.
---

# magents

Help the user manage their own agents and durable sessions from a terminal. Prefer the installed CLI's `--help` and subgroup help over reconstructing HTTP requests or writing wrappers. Resource results are JSON on stdout; streamed assistant text is stdout, session IDs and errors are stderr, and failures exit nonzero.

## Establish the boundary

- Production defaults to `https://agents.moesegfault.dev`. Inspect whether `MAGENTS_API_URL` overrides it. For explicitly requested staging work, use `magents --api-url https://agents-staging.moesegfault.dev ...` consistently, including login. Credentials are origin-scoped; never silently switch environments.
- Match external actions to the user's request. Offline help, template generation, and validation do not authorize publication, execution, deletion, diagnostic upload, or paid resource use.
- Keep authentication/provider secrets out of prompts, chat, TOML, command arguments, and logs. Let the human perform browser login and hidden provider-key entry; use protected secret injection for automation.
- A broken stream leaves execution outcome uncertain. Preserve the session ID, inspect status, and never blindly replay input.
- Expanded branches, Conversations, native transport and tool-service config are capability/version dependent. A source parser or newer CLI is not evidence that staging or production has activated them; inspect the selected release and realm before using candidate interfaces.

## Read only the relevant guide

- CLI setup, login, automation credentials, or custom model endpoints: [account and BYOK](references/account.md).
- Create, edit, validate, publish, or transfer TOML, including native model transport and independent tool-service routes: [configuration](references/configuration.md).
- Manage server-side named instructions: [personalized skills](references/personalized-skills.md). These are not filesystem `SKILL.md` packages; this folder is not automatically uploaded or discovered by the cloud harness.
- Run, continue, steer, close, or resume work: [sessions](references/sessions.md).
- Create/branch/recover a Response, stream typed events, return local results, or choose the appropriate SDK-only advanced interface: [Responses](references/responses.md).
- Upload/manage Space files or persist/discover/restore workspace versions: [Space](references/space.md).
- Retrieve conversations or workspace files: [exports](references/exports.md).

Use local CLI discovery first; consult the linked official contracts for deeper behavior rather than loading every guide.
