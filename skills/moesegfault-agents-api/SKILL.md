---
name: moesegfault-agents-api
description: Integrate applications with moeSegFault Agents through OpenAI-compatible Chat Completions, Responses, Files, and Space APIs. Use for SDK/HTTP clients, not operating magents or developing the platform repository.
---

# moeSegFault Agents application integration

Build a service client, not an upstream-model proxy. Preserve the application's authentication boundary. Keep platform credentials on the trusted backend, never in browser storage or frontend code.

## Establish the contract

- The production SDK base is `https://agents.moesegfault.dev/v1`; explicitly selected staging uses `https://agents-staging.moesegfault.dev/v1`. Raw HTTP starts at the matching origin and includes `/v1` exactly once. Do not mix realm credentials or IDs.
- `model` is an **owned agent ID**, never an upstream model name. Chat Completions remains the text-only user-input subset; do not import Responses capabilities into its request schema.
- The public client protocol stays OpenAI-compatible even when the agent uses an Anthropic BYOK endpoint. An upstream provider key and a platform bearer key are different credentials.
- Disable automatic execution retries. Persist response/session recovery identities immediately: a failed observer does not establish that the durable run failed.

## Read only the needed reference

- Maintaining Chat Completions, session steering, or its stream recovery: [Chat client contract](references/client-contract.md).
- Implementing Responses sync/stream/background runs, continuation, or local functions: [Responses contract](references/responses.md).
- Selecting agents/models, onboarding BYOK, or determining account-management permissions: [configuration and credentials](references/configuration.md).
- Uploading Files, using the account Space, preserving workspace versions, or delivering transcripts/artifacts: [files and exports](references/exports.md).

Verify the actual journey with local mock transport: creation, continuation, disconnect recovery without replay, and server-side credentials. Match paid execution to the user's request; offline work does not authorize a live smoke test. Report behavior and remaining deployment prerequisites, not merely a successful import.
