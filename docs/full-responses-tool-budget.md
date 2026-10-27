# Response-wide built-in tool allowance

`max_tool_calls` is enforced by the Session harness rather than trusting an
upstream provider to honor a copied request field. The allowance is shared by
all platform-dispatched tools in one Response, including pinned harness and
workspace tools and declared hosted-tool aliases. Caller-local function/custom
handoffs are not executed by the platform and do not consume it.

The canonical state is a default-empty set of stable execution keys on
`RunState`, not a count plus a second retry ledger. A new authorized dispatch
adds its key and persists before the first tool effect. Recovery of the same key
does not spend another slot, but persists again before effect reconciliation in
case the previous write failed while leaving a key in warm memory. This set is
only an allowance reservation; each implementation's existing effect/outcome
fence still decides whether executing or reconciling is safe. Unlimited legacy
runs do not add keys or gain an extra persistence boundary.

After exhaustion, attempted tool calls return a bounded tool-result error and
perform no tool effect. Subsequent provider requests omit server definitions,
including hosted functions rendered from native declarations; caller tools stay
available. The model can still produce a final answer. No invented
`incomplete_details.reason` is used. The normal completed Response echoes the
requested limit, while a new Response starts a new set even when it continues
the same Session and the provider reuses an upstream call ID.

The set defaults empty when reading older durable state. Existing unlimited Chat
and Responses execution retains its behavior. The set is bounded by the admitted
request's integer limit, and denied or unknown tool names never add entries.

## Verification

- `session_tool_budget.rs` tests stable key reuse, cross-round IDs, new-response
  scope, unlimited legacy state, and property-based serialization/recovery traces
  that never exceed the limit.
- `tests/worker/responses-tool-budget.mjs` exercises the actual Rust Worker and
  DO through an encrypted synthetic BYOK provider route: two calls in one model
  round, another disobedient call after definitions are removed, completed output,
  retained caller functions, cold retrieval without redispatch and a fresh budget
  on the next Response. It must be enlisted in the integrated Worker CI runner;
  syntax checking alone is not an execution receipt.

Primary contract:
[OpenAI Create Response](https://developers.openai.com/api/reference/resources/responses/methods/create)
defines a response-wide allowance for built-in calls and ignores further model
attempts. The implementation applies that allowance at the platform execution
boundary instead of treating custom caller handoffs as paid built-in work.
