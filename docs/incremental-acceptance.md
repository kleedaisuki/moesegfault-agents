# Incremental release acceptance

The authoritative user policy is to reuse already passing evidence and spend new execution only on new capabilities, changed behavior, or an evidenced failure. A green deployment is not a claim that old tests were rerun, and a partially verified historical job is not a fully accepted candidate. Production and public release still require root's final clearance.

## Evidence ownership and lifetime

| Evidence | Meaning | Reuse boundary |
| --- | --- | --- |
| Sealed historical feature receipt | That named behavior passed at its recorded source | Keep the original source, dependency context and outcome; do not relabel it passed at a newer source |
| Component build baseline | A known artifact and compiler/dependency input set | Reuse the artifact only when sorted Git blob/mode/path fingerprints match, the declared source/run/artifact identity is valid and the archive remains available |
| Selected current result | The exact planned new/repair selector actually ran successfully | Match source, arguments/environment, positive executed-test count and current CI job |
| Partial historical Worker proof | Specified successful build/library phases despite failed integration leaves | Preserve the actual failed job conclusion; all declared unresolved leaves require current repair receipts |
| Staging increment receipt | Selected genuine new user behavior was exercised | Aggregate with the sealed baseline; do not pay to replay good cases merely to collect other evidence |

`scripts/verification-baselines.json` is the reviewed catalog. The incremental CI plan names component fingerprints, planned selectors, artifact source runs and unresolved failures. Planned checks are not results. `verification-<job>` artifacts carry actual source-bound result receipts, and `scripts/deploy-provenance.mjs` recomputes the plan from committed source before accepting them.

The deployment gate checks normal trusted repository runs, required current jobs, sealed historical jobs or exact successful partial phases, positive current execution counts, known-failure remediation, and immutable artifact metadata. Skipped jobs remain skipped. It does not require all six historical jobs to run again, and it never turns `no_selected_tests` into a current test pass.

## Artifact and runtime provenance

Worker, dashboard and daemon may originate from different explicitly recorded successful component sources when their relevant source/dependency fingerprints are identical. The deployment manifest records each actual artifact source/run/ID. `BUILD_ID` remains the real Worker artifact source; `DEPLOYMENT_ASSEMBLY_SHA` separately records the candidate configuration. Health validation expects the real Worker identity, not a newer UI/docs-only HEAD. Public tag/native binary sources are independently declared and verified. Diagnostic runs cannot substitute for release evidence.

The daemon is loaded from the accepted archive and its original receipt/source before publication. `CONTAINER_SOURCE_SHA` selects that immutable source tag; it defaults to the candidate SHA for existing callers. Both `Session/basic` and `Session4G/standard-1` apps receive exactly the same tested image. There is no promotion rebuild, second harness or mutable image fallback.

The new 4 GiB profile is not accepted by configuration labels alone. Before paid Code Interpreter work, obtain actual deployed application/SKU and owner-scoped daemon capacity proof. Standard-1 means 0.5 vCPU capacity, 4 GiB memory and 8 GB disk. The approved 21 microUSD/second tariff is a published **platform allocation/reserved-capacity** price, revision `2026-10-25-platform-standard4g-v1`, not a measurement of idle CPU or the exact Cloudflare invoice. Preserve legacy basic pins, leases and price bytes. Requests for 16/64 GiB remain explicitly unavailable until genuine capacity is provisioned.

## Explicit staged profiles

Deployment defaults to `acceptance_profile=none`: publish verified components and record `not_selected`, without replaying legacy paid Chat/time/file CRUD, old browser/CLI journeys or the 80 KB compaction trajectory.

- `direct-deepseek-usage-probe`: the original single authorized research observation has its own operator job, with no app deployment or Cloudflare credentials. Its pushed helper/workflow component must have actual positive scoped check receipts; unrelated Worker/container failures do not block that research proof. A globally named GitHub intent artifact is written before the single provider request, so a new workflow cannot silently evade an ambiguous prior attempt. Capture only real usage keys/integers and bounded model metadata. Missing cache-write is not zero. This profile records an observation, not platform or compact acceptance.
- `responses-increment`: the first genuinely new native Responses/Conversations/fork/auxiliary/WebSocket cluster, using direct DeepSeek and a bounded potential-admission budget. Use cheap ownership/count/pin preflights before model work. Hosted dispatch is a separate later increment, not implicitly accepted by this profile.

Selected paid profiles reject Actions reruns. After uncertainty, inspect the same accepted response, operation, event cursor and ledger; do not regenerate work. Successful subfeature evidence is retained if another new case fails. Before a later paid run, select only the unresolved/new behavior rather than repeating this whole first-increment profile.

All paid language-model acceptance uses direct DeepSeek with an actual provisioned model and immutable Session pin. The separate Seedream 5 image service may use OpenRouter. This testing policy does not remove product BYOK capabilities.

## Current evidence boundary

The old compaction/protected-marker/authentic replay and browser/native transfer receipts remain valid historical evidence. The older linear/no-forks acceptance boundary is not the expanded product objective. Historical and concurrent branches, Conversations/items, current SDK fields, standalone compact/count, WebSocket lanes/steering, hosted tools, physical storage sharing and new resource profiles still need their own named evidence.

The b1 Worker run `40191955285` built Wasm and passed 363 library cases, while three leaf defects caused its integration job to fail. The 55/60 aggregate includes duplicate parent failures; it is not a passing candidate. Its known passing component/selector evidence can be sealed, and only the actual unresolved leaves and new changes need execution. Never replay those known passing 363 cases or 51-plus journeys merely to obtain a globally green label.

A coherent compiled API increment may be staged to learn its genuine native usage and new API behavior before hosted dispatch is complete, provided unavailable capabilities reject honestly before paid work. Such staging does not complete the full objective. Final release requires the aggregate named capability receipts, financial/resource closure, the exact public artifact gate and root's explicit decision. Do not inspect legacy production profiles/data or apply old release drafts before that clearance.

Historical evidence location: the `.temp/` receipt paths in the following
sections refer to the preserved `.cache/v2-release-worktree` checkout. They are
not new executions in this integration checkout; no identity or credentials move.

## Collector lifecycle correlation repair (2026-10-26)

The final db58 staging capture contains all three `run.started` and all three
`provider.completed` events with the expected attempt trace. Their absence in an
early live read was tail-delivery delay, not demonstrated backend trace loss.
The original global `events` list omitted two untraced `run.completed` records,
although all three were already retained in the same owned invocation's
sanitized `native.events`. The remaining local capture has the same collector
aggregation defect (zero global terminals versus two native terminals).

The metadata collector generator now merges only sanitized, untraced lifecycle
records linked by run/session identifiers to exact matched events in that same
invocation. It preserves richer exact-match fields, rejects foreign runs/traces,
and does not invent a trace or a success outcome. The focused offline check now
covers the actual `consume` path, not only the merge helper. Existing captures
were reconciled offline: db58 terminals 1 to 3; remaining-local terminals 0 to 2;
all other counts unchanged, no duplicate events, idempotent merge. Both original
receipt hashes remain unchanged. The separate safe supplements are under
`.temp/stage-delivery/collector-correlation-repair/`; no tail, app, or model
request was started for this repair.

The missing later warm-WebSocket log is not explained by that result. Do not
replay accepted model work or patch backend tracing on this evidence. Inspect
request/run/provider/tool correlation during the next already-needed new hosted
staging journey, using a new bounded metadata capture. Previously stopped
collectors are not restarted.

Source-only inspection at the actual deployed Worker `8578792` confirms that
`response_websocket_executor::Harness::create` derives a child of the connection
trace for each frame and passes its `RouteContext` into canonical admission;
`session_responses::create_response` stores the supplied traceparent on the new
run. `run.completed` intentionally contains session/run IDs without a trace in
that source. This explains why same-invocation terminal correlation is needed,
but does not identify the cause of a missing later warm-WebSocket invocation.

## Narrow live Code increment driver

`tests/e2e/hosted-code-sdk.mjs` extends the existing SDK acceptance helpers rather
than introducing another harness. It is prepared, not live accepted. Supply the
accepted assembly SHA, exact downloaded deployment provenance, existing owned
platform key in process memory, a unique trace seed, project-managed current
Python SDK, a new project-local receipt path, and `--allow-model-call`.

The driver first checks actual Worker/daemon artifact provenance and model-free
hosted-tool admission, then submits one new bounded direct-DeepSeek Response
(`max_tool_calls=1`, auto/default 1 GiB, Bash workspace disabled). It requires real
native Code events/CID/logs, a unique published nine-byte CSV with an exact SHA,
and current Python SDK validation of the original response/events through stdin
only. Its real-resource sequence is Session archive, CID still running, explicit
standard Containers DELETE, CID 404, and unchanged published File bytes. No
natural idle-clock wait, 4 GiB claim, accepted-POST replay, or legacy core journey
is included. Receipts retain identifiers, hashes and bounded metadata, not model
code, prompts, provider secrets or raw SSE. Behavior success remains explicitly
`financial_postflight_pending` until the owned Session/run ledger, holds, Code
allocation-release timestamp and compute lease are checked separately.

## Orthogonal staging runtime flags

Deployment now accepts `runtime_features=configured|none|web_search` independently
of `acceptance_profile`. `configured` preserves existing rendering defaults;
explicit overrides are staging-only. `web_search` enables only the existing
Worker web-search runtime and platform-search capability. Code, image generation,
file search and 4 GiB apps remain disabled. `acceptance_profile=none` still records
`not_selected` and never invokes the old paid core runner.

The same finite runtime selector controls rendering and compatible legacy-daemon
provenance. A pure Worker search increment may retain actual b1 while its
container-dependent new tools are disabled. The existing container scope/status
and immutable source/image identities are preserved; `worker_runtime_features`
explicitly records the separately enabled Worker feature. An unverified Code or
image feature cannot enter through that compatibility path. Production rendering
and default old callers are unchanged.

`tests/e2e/hosted-search-sdk.mjs` is the narrow prepared live driver, not a live
acceptance result: one new direct-DeepSeek outer Response, one declared platform
search, low context, official Rust domain filter, source inclusion and
`max_tool_calls=1`. It verifies the actual deployed 750-microUSD tariff revision,
model-free admission, native search phases/sources, original current-SDK wire
through stdin, and GET-only canonical JSON/event retrieval. It does not replay an
accepted POST, invoke the consumed usage observation, or enable compute/image
work. The independently scoped actual service ledger and financial closure are
required after its behavior receipt. Three new driver contracts and four new
runtime-feature deployment contracts passed locally; no controlled six-case
search or old eight-call model trajectory was rerun for this preparation.

### Search-only live closure

Deployment `40298168131` selected the orthogonal Worker `web_search` feature with
`acceptance_profile=none`; its component identities remain actual Worker `2cc1e0c`,
assembly `0074abd`, dashboard `5266267` and daemon `b1edf97`. The separate single
real platform-search journey passed native phases/sources, original strict SDK
wire, GET-only canonical retrieval, exact 469-token count/provider parity and
one independently settled 750-microUSD service unit. The new Session total was
2,037 microUSD, with zero new holds/pending observations after normal cleanup.
The same request supplied complete request/run/provider/tool correlation and
actual native/global terminal parity; all capture processes stopped without raw
retention. Full source/financial/driver-repair boundaries and receipt paths are in
`docs/full-responses-search.md`. Neither the zero-submission count-preflight
failure nor the original failed CI was relabeled successful, and no accepted paid
operation or legacy core trajectory was replayed.

## Composed selection and immutable artifact baselines

A catalog entry may now name a `selection_base` with `composed_accepted` status,
an ancestor source and explicit references to the original scoped receipts. This
is a change-selection boundary, not a claim that that combined commit was built
or that every historical CI job succeeded. Its unresolved checks are the current
work; old artifact failures are not an eternal replay list after the actual
feature journeys have been closed on retained binaries.

Artifact source/run/ID/fingerprint and positive build steps remain unchanged and
are verified independently. Any changed binary is rebuilt from the new exact
source. Native type/lint package selection still compares that real artifact
source, not the composed selection source. Deployment provenance records both
identities and rejects a selection not present in the committed catalog.

The first composed boundary closes the separately recorded caller-tool, owned
Code/lease, Search, retrieval/read-recovery, image, sealed-native and default
DeepSeek compaction increments. The next regular build selects only the new
staging native FileSearch configuration and its zero-network intent guard. It
does not turn controlled acceptance into a live CF embedding claim; that remains
the operator's next single, zero-LLM journey after exact-source deployment.
## Code rollout selection (2026-10-26)

The next normal source build uses composed selection `24e5bea`, with the original
Code topic executor/real Python/owned-guest receipts, Vision exact-count receipt
`40327906021`, compact budget receipt `40327392475`, and repaired normal source
fixture receipt `40328309649`. The catalog retains their actual sources and
failed aggregate conclusions where applicable. This is not a claim that the
combined selection source was compiled or live Cloudflare Code was accepted.

Only the new cross-interface plain compact bound, six executor UI form cases,
two Code deployment/provenance cases and five narrow staging-driver contracts
remain selected. Changed Wasm, native packages, daemon image and dashboard are
built under normal CI from the exact new source; artifact/dependency fingerprint
gates are unchanged. The new compact media transport is not part of this source.

## Independent feature rollout and corrected capacity source

The next composed selection boundary is `afba8e9`, not a fabricated compiled
artifact. Exact normal component baselines now come from successful source CI
`40338717364` / `1b7f2cd`; its Worker/container/dashboard artifact IDs are pinned
individually. Controlled native-window, context quota/purge/count, media, MCP,
indexing-resume and programmatic receipts retain their original sources and
partial aggregate outcomes in the catalog. They are not replayed for a green
combined label.

New work selected after that boundary is exactly five relevant CLI binary cases
(four new cases and the changed progressive-help contract), three native capacity
source/discovery cases, one free native document price/capability guard, and twelve
new or materially changed deployment/driver cases. No old compiled Worker
journey, managed Python cell, SDK trajectory or provider request is selected.
Changed Wasm/native/daemon inputs build normally; the identical dashboard reuses
its exact `1b7f2cd` artifact.

The additive `web_search,file_search,image_generation` staging profile keeps both
verified daemon application bindings for existing cleanup but advertises no Code
memory profile and no Code runtime. Image activation still requires successful
normal CI and normal verified container provenance, never the old disabled-runtime
exception. This lets independent new user capabilities ship while managed Code
gets its separate actual-runtime acceptance. The later Code selector can enable
its already built corrected source without replaying any accepted operation.

### Single namespace-fixture repair without positive-case replay

Run `40349988842` at `1d1c3a2` built the actual Worker (`12663258034`) and daemon
image (`12663702294`), passed the one new free native document guard and all
 twelve selected tooling cases, and skipped the identical dashboard. Its native
receipt `12663691578` records five actual CLI binary cases and two new capacity
cases as `passed_current`, each with one executed test. The aggregate native job
failed only because the namespace fixture's synthetic mount path contained
literal `../../`, which the real absolute-path validator correctly rejected.
The subsequent native clippy step was skipped, not passed.

The correction canonicalizes that synthetic fixture path only. Its composed
selection references the original successful leaves and keeps only
`namespace_ancestor_mount_finds_the_actual_process_not_a_matching_sibling_limit`
unresolved. The exact Worker and dashboard artifacts are reused from their real
sources. The daemon is rebuilt because its tracked test-file fingerprint changed;
normal native lint/type checking remains required. No passing CLI/capacity/tooling
case, old Worker journey or provider request is replayed for the repair.

## Recovery-source composition after actual managed Code delivery

Selection `35676de` composes the separately accepted zero-generation quota/count
negative scopes, private native-source serialization, remote/cache metadata,
legacy MCP negotiation, live programmatic client continuation, and real slow-I/O/
lost-ACK Conversation recovery receipts. Original failed aggregate conclusions
and exact-artifact local tails stay explicit. These are selection receipts, not
a claim that the combined source or a public remote server was already exercised.

The next normal plan builds only the changed Worker and runs the four new cheap
native-release selector contracts. Native/daemon `3539c1c` and dashboard `1b7f2cd`
retain their exact artifact identities. No accepted Code cell, old CLI binary
case, native-window generation, compaction, MCP or programmatic model journey is
replayed. The original failed Conversation response is recovered only through
its existing standard metadata DELETE/purge and canonical never-activated fence;
its old resource scope settles from its original observed ledger, never from a
new model request or invented refund.

### Structured diagnosis survives collection

Future owned metadata collectors preserve `conversation.phase` and bounded
operation/conversation/request/grant/span identifiers, phase and route, lock-wait,
D1 funding/RPC/observation durations, pending/activated/deadline/generation, and
upstream status/request/generation identifiers plus byte counts, codec and image
dimensions. Same-invocation records keep the already-sanitized structured fields
instead of reducing them to Session/run IDs. Merge identity includes phase,
operation and span so distinct phases in the same millisecond do not collapse.
The process-local reducer extension leaves old captures and the shared classifier
unchanged. Its offline actual reduction/correlation probe retained all 21 expected
structured fields and no Authorization/prompt fields, with zero network clients.
Full sensitive execution bodies belong in authorized encrypted short-retention
receipts, not plaintext global tail output. Missing source-emitted upstream detail
cannot be reconstructed by pretending that a generic error is sufficiently precise.

## Actual staging recovery after the slow-I/O guard repair

Normal CI `40369812194` and deployment `40445875291` succeeded for assembly and
Worker `02d483b024386874c3a9c64d476cce0cec0c1130`. The daemon remains the exact
accepted `3539c1c` artifact and dashboard the exact accepted `1b7f2cd` artifact.
The feature allowlist is `web_search,file_search,code_interpreter`; Image remains
disabled pending the separate response-receipt repair. Deployment selected no
old model journey or Code cell, and does not itself claim product acceptance.

The original failed Conversation submission
`response-59ded45a-aba8-46a1-8582-69b1fc24921c` was not resubmitted. Its recovered
Session was already closed with no model execution. Normal metadata DELETE and
explicit purge returned HTTP 200 on the new Worker. The original resource scope
`conv-response:resp_78a70f2f64e24b659f72c9cbf73e67c0` settled **137 microusd**
from its existing observed ledger; its original 20,000-microusd reservation was
not replaced or manually edited. All 20 observations are observed, the output
escrow is released with zero remaining bytes, and body allocations are physically
removed before release. The original test Agent was deleted normally afterward.
There were zero new Response submissions or model calls for this recovery.

This is also real Cloudflare evidence that a purge can now finish beyond the
old 30-second initialization-guard limit: metadata DELETE took 22,392 ms and
purge 49,151 ms. Those are unacceptably slow management operations, not a
performance success. They justify a narrow phase-attributed funding/D1 latency
investigation without replaying any paid request. The local authoritative receipt
is `.temp/features-staging/context-original-recovery-02d.json`; it preserves the
exact original scope, API timings, capacity state and accounting rows. The old
failed response remains an old failure, not a newly completed generation.

### New real-provider summary did not complete

The fresh, separately owned Context operation on Worker `02d483b` passed the
previous admission/reset boundary: the client received HTTP 200 and the actual
Session/Response headers. It then failed in `compaction_accounting` with the
specific runtime code `compaction_incomplete`, not another Conversation guard
reset. Retained Response `resp_032ceda34a484ac2ba5ee5642e14abc3` is `failed` with
no output. This is not a successful automatic compaction and must not be replayed
to manufacture one. The original request key is
`response-dc47885c-7389-4eeb-9e63-864c5e2aef9a`.

The owned Session closed normally and its temporary Agent was deleted. A scoped
read found its single direct-DeepSeek reservation settled at 655 microusd, the
Conversation resource scope at 305, execution resources at 64 and maintenance at
7: **1,031 microusd** across these exact scopes, with no reserved hold or pending
observation. This is the failure's observed accounting, not a full acceptance
bill or evidence of a second answer generation. Independent metadata DELETE and
purge cleanup of this fresh failed Conversation remains a separately recorded
operation.

The operator now records the actual terminal event/status/error, incomplete
reason, usage, output types and event count before assertions. Its known local
assertion failure also retains the assertion and phase, instead of reducing every
failure to `ERR_ASSERTION`. This change needs no paid replay; a syntax check
covers the edited JavaScript. Production diagnostics still require the deeper
provider-terminal reason behind `compaction_incomplete`.

The new phase collector also found a concrete management-latency mechanism:
19 completed Conversation D1 phases totalled 63,315 ms, of which actual D1 RPC
was 3,314 ms, funding 13,392 ms and observation bookkeeping 46,609 ms (73.6%).
These are additive completed-phase durations, not end-to-end wall time. The
phase summary is `.temp/features-staging/context-phase-summary-02d.json` and
owned trace `.temp/features-staging/trace-context-repaired-02d/receipt.json`.
The next optimization should remove redundant sequential accounting round trips
without discarding observations or weakening capacity/funding fences.
