# Funded Conversation operations

`conversation_meter.rs` reuses the platform's existing resource reservations,
observations, exact tariff arithmetic and ledger. It creates no model invocation,
model price, R2 object or synthetic storage quantity.

## Ownership and scopes

| Scope | Funding lifetime | Parent measurement |
| --- | --- | --- |
| Public CRUD/read/snapshot | One account operation, settled after a definite response | Actual Worker operation wall interval |
| Response admit/activate/release | Stable `conv-response:{response_id}` account cleanup hold | A fresh interval for each actual RPC, never the whole agent execution duration |
| Admitted terminal append | Existing Session run, before its financial cutoff | The Session already measures its own step; only the additional Conversation actor and routing query are added |

The stable cleanup hold is reserved before admission and remains reserved through
activation. Failed later model-credit admission cannot require fresh balance just
to clear an activated writer lease. Canonical terminal/rejection release calls
`finish_response` instead of ordinary `finish`; a previously settled cleanup scope
can skip repeated actor dispatch. Existing funding is read and verified rather
than re-reserved at a changed deployment's default amount. Its owner, account-only
binding, maintenance key and immutable request hash must still match.

Expiry of an unactivated admission still requires the authority/recovery owner to
establish that the candidate cannot activate before releasing its cleanup hold.
This module does not infer that proof from age or fabricate zero measurements.
An abandoned pending lifecycle hold therefore remains an explicit recovery item
until that canonical cleanup is integrated.

## Measurement and trust boundary

The authenticated router begins funding before its first user-data routing query,
including lookups that return 404. Routing lookup and actor dispatch share one
operation; every D1 statement goes through `query`, which declares its observation
before execution and records `rows_read`/`rows_written` from D1 result metadata.
Returned-row counts and affected-row guesses are never usage measurements.
Cloudflare's [D1 result contract](https://developers.cloudflare.com/d1/worker-api/return-object/)
defines these as scanned/written rows in query metadata, separately from returned
results and `changes` (checked 2026-10-25).

Only freshly constructed internal requests receive
`X-Agents-Conversation-Meter`. The bounded JSON grant contains the funding run,
optional Session binding and unique actor-attempt event; it is not an independent
authentication token. The actor independently checks its existing owner binding.
Before reading/writing Conversation user data, it also checks that the exact
owner/session reservation is still reserved and that this attempt has a pending
Durable Object wall observation. Settled funding and an already-observed grant
cannot authorize a replay. No client-supplied header is forwarded by the router.

Every actor attempt receives a fresh event, and the actor measures its handler's
actual wall interval. A retry after ambiguous transport does not overwrite the
first attempt's unknown measurement. Resource-ledger/control-plane queries follow
the platform's existing non-recursive metering convention.

## Failures and user-visible behavior

An observation is declared before its side effect. Missing D1 metadata, ambiguous
transport or an accounting acknowledgment loss leaves the observation pending and
its budget held. A definite committed HTTP result is preserved when settlement
fails: `X-Agents-Accounting-Status: pending` and the operation ID expose that state
without turning a successful mutation into a retriable 5xx. Accounting failure
does not authorize repeating user-data mutations or agent execution.

`ConversationState` owns SQLite/chunked user data, not R2 retained bytes. The
current published platform tariff has active Durable Object wall time but no
separate DO-storage byte-time resource. This implementation does not mislabel DO
data as R2 storage or invent a new fee. Storage limits and any distinct retained
DO-storage tariff remain an explicit product/platform accounting decision.

## Verification

`tests/test_conversation_meter_sql.py` executes the production funding gate with
all real migrations. It covers account/Session/owner isolation, missing funding,
unknown-attempt holds, settled reservations and already-observed grant replay.
The actual compiled-Worker Conversations journey adds measured routing-miss
assertions and funds its direct private-authority fixture through real bounded
reservations; it does not disable the production grant requirement. Rust tests
cover bounded grant parsing, injected labels and Session measurement ownership.

Only formatting, syntax and focused SQLite tests were run locally. The new Worker
artifact and end-to-end resource journey are validated by the root-owned Actions
checkpoint; an older artifact cannot establish this implementation's behavior.
