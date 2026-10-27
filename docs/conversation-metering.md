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

## 2026-10-27: measured accounting round-trip amplification

The actual `02d483b` staging recovery had metadata DELETE 22,392 ms and explicit
purge 49,151 ms. Those two requests did not carry a trace header, so their internal
phase attribution is unknown. The subsequent new, traced Context journey provides
causal evidence without replaying the old request:

- Trace `5b6427be404ea4644f44206dfef55648`, source `02d483b`.
- Nineteen completed measured D1 operations total 63,315 ms: actual application
  D1 RPCs 3,314 ms (5.2%), measurement admission 13,392 ms (21.2%), measurement
  publication 46,609 ms (73.6%). Individual actual D1 queries took 165–194 ms.
- `require_d1` performs four sequential D1 binding round trips; `record_d1`
  performs fourteen more. Consequently one measured query costs nineteen D1
  round trips despite having only one application query.

Conversation operations now opt into `resources_d1_batch.rs`. Two observation
admissions and their identity verification share one native D1 batch before the
application effect. Actual `rows_read` and `rows_written` from its returned D1
metadata are then recorded, ledger-checked and marked observed in one subsequent
batch. A measured query therefore costs three binding round trips instead of
nineteen. Other ResourceMeter callers remain unchanged.

The same owner/session binding, pinned exact tariffs, immutable ledger event keys,
append-budget triggers and pending-observation settlement fence are preserved.
Ledger append and observed transition are now in a single transaction. An unknown
application result remains pending; a lost accounting acknowledgement can retry
that original event without charging or mutating the application twice. An
existing conflicting ledger does not authorize marking its observation complete.

Cloudflare documents native [D1 batch transactions](https://developers.cloudflare.com/d1/worker-api/d1-database/#batch):
ordered statements share one call and roll back together on error. There is no
new custom SQL transaction protocol or financial state machine.

Focused verification is `tests/test_d1_accounting_batch_sql.py` (seven new cases
using the exact production SQL and every migration) plus
`tests/worker/conversation-accounting-perf.test.mjs` (actual compiled Rust Worker,
SDK management operations, D1, fixed 20 ms actor-DB round-trip delay; optional
retained `02d483b` baseline via `MAGENTS_BASELINE_WORKER_DIR`). The latter verifies
same logical work, metadata-delete retention, explicit physical purge, exact
settlement and zero model/external calls. Fixed-delay figures are a controlled
mechanism comparison, not a staging latency SLO. Compilation and actual Worker
execution are required before claiming this implementation is accepted.

### Controlled compiled-Worker result

Focused Actions [40447050803](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40447050803)
passed on proof source `7ed16536f9fa2dc1b49f95e64522f21bcb86c0ea` (pure product
change `08e17163bbc49989aafad7d0c360b1de4827956f`). Worker artifact `12709510201`,
verification artifact `12708982895`. The source-only CI selector and baseline
artifact download are not product changes and must not be merged.

The one new actual Rust Worker/SDK/D1 benchmark executes equivalent management
work against the retained `02d483b` Worker and the optimized Worker. Both use the
same 20 ms delay per actor D1 binding call; account funding, native database
statements, immutable content publication and actual metering execute normally.

| Measurement | Retained baseline | Batched | Change |
| --- | ---: | ---: | ---: |
| SDK metadata update elapsed | 2,307.987 ms | 558.062 ms | -75.82% |
| Metadata update actor D1 round trips | 87 | 23 | -73.56% |
| Metadata update application SQL statements | 5 | 5 | unchanged |
| Full management journey actor D1 round trips | 309 | 85 | -72.49% |
| Full management journey application SQL statements | 17 | 17 | unchanged |
| Pending observations / reserved funding at completion | 0 / 0 | 0 / 0 | unchanged |
| External / model calls | 0 / 0 | 0 / 0 | unchanged |

The journey includes metadata deletion retaining its original item, then explicit
purge, and verifies normal exact-ledger settlement. Seven new production-SQL
contracts passed in the same run (0.379 s), covering account/Session isolation,
same-event replay, conflicting ledger/quantity, missing funding, unknown result
holds, rollback on budget failure, and real zero usage. The actual Worker
comparison took 15.5 s; the CI job's major elapsed costs were tool installation
(118 s) and release compilation (207 s), not broad regression execution.

These controlled elapsed times demonstrate the causal round-trip mechanism,
not production or staging latency. A fresh model-free management operation on a
subsequent composed deployment is the remaining field measurement; the original
paid Context request must not be replayed for it.
