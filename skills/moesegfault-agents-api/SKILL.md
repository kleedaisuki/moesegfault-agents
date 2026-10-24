---
name: moesegfault-agents-api
description: Integrate applications with the existing moeSegFault Agents service using its OpenAI-compatible API, owned sessions, streaming, and exports. Use for SDK or HTTP client implementation, not operating the magents CLI or developing the platform repository itself.
---

# moeSegFault Agents application integration

Build a client of the deployed service, not a transparent upstream-model proxy. Start with the application's existing authentication boundary and intended user journey. Keep platform credentials on the trusted backend; never put bearer keys in browser storage or shipped frontend code.

## Establish the contract

- The production SDK base is `https://agents.moesegfault.dev/v1`; explicitly selected staging uses `https://agents-staging.moesegfault.dev/v1`. Raw HTTP starts at the matching origin and includes `/v1` exactly once. Do not mix realm credentials or IDs.
- Chat Completions `model` is an **owned agent ID**, never an upstream model name. Input is new text-only user messages. Instructions, tools, skills, and model policy are configured server-side and pinned per session.
- The public client protocol stays OpenAI-compatible even when the agent uses an Anthropic BYOK endpoint. An upstream provider key and a platform bearer key are different credentials.
- Disable automatic execution retries. Preserve the original session ID before long work: a failed observer does not establish that the durable run failed.

## Read only the needed reference

- Implementing SDK calls, continuation, busy-session handling, or stream recovery: [client contract](references/client-contract.md). It includes a runnable Python example and lifecycle request shapes.
- Selecting agents/models, onboarding BYOK, or determining account-management permissions: [configuration and credentials](references/configuration.md).
- Delivering transcripts or workspace artifacts: [exports](references/exports.md).

Verify the actual client journey with local mock transport first: session creation, streamed continuation, disconnect recovery without replay, and credentials staying server-side. Match live execution and its possible resource charges to the user's actual request; offline integration work does not authorize a paid smoke test. Report the integration behavior and remaining deployment prerequisites, not merely a successful import.
