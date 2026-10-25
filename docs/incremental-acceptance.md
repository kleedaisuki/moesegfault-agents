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

Worker, dashboard and daemon may originate from different explicitly recorded successful component sources when their relevant source/dependency fingerprints are identical. The deployment manifest records each actual artifact source/run/ID; the deployment `BUILD_ID` labels the candidate configuration, not a claim that reused binaries were rebuilt. Diagnostic runs cannot substitute for release evidence.

The daemon is loaded from the accepted archive and its original receipt/source before publication. `CONTAINER_SOURCE_SHA` selects that immutable source tag; it defaults to the candidate SHA for existing callers. Both `Session/basic` and `Session4G/standard-1` apps receive exactly the same tested image. There is no promotion rebuild, second harness or mutable image fallback.

The new 4 GiB profile is not accepted by configuration labels alone. Before paid Code Interpreter work, obtain actual deployed application/SKU and owner-scoped daemon capacity proof. Standard-1 means 0.5 vCPU capacity, 4 GiB memory and 8 GB disk. The approved 21 microUSD/second tariff is a published **platform allocation/reserved-capacity** price, revision `2026-10-25-platform-standard4g-v1`, not a measurement of idle CPU or the exact Cloudflare invoice. Preserve legacy basic pins, leases and price bytes. Requests for 16/64 GiB remain explicitly unavailable until genuine capacity is provisioned.

## Explicit staged profiles

Deployment defaults to `acceptance_profile=none`: publish verified components and record `not_selected`, without replaying legacy paid Chat/time/file CRUD, old browser/CLI journeys or the 80 KB compaction trajectory.

- `direct-deepseek-usage-probe`: one explicitly authorized operator observation after coherent CI and Ops clearance. A globally named GitHub intent artifact is written before the single provider request, so a new workflow cannot silently evade an ambiguous prior attempt. Capture only real usage keys/integers and bounded model metadata. Missing cache-write is not zero. This profile records an observation, not full compact acceptance.
- `responses-increment`: the first genuinely new native Responses/Conversations/fork/auxiliary/WebSocket cluster, using direct DeepSeek and a bounded potential-admission budget. Use cheap ownership/count/pin preflights before model work. Hosted dispatch is a separate later increment, not implicitly accepted by this profile.

Selected paid profiles reject Actions reruns. After uncertainty, inspect the same accepted response, operation, event cursor and ledger; do not regenerate work. Successful subfeature evidence is retained if another new case fails. Before a later paid run, select only the unresolved/new behavior rather than repeating this whole first-increment profile.

All paid language-model acceptance uses direct DeepSeek with an actual provisioned model and immutable Session pin. The separate Seedream 5 image service may use OpenRouter. This testing policy does not remove product BYOK capabilities.

## Current evidence boundary

The old compaction/protected-marker/authentic replay and browser/native transfer receipts remain valid historical evidence. The older linear/no-forks acceptance boundary is not the expanded product objective. Historical and concurrent branches, Conversations/items, current SDK fields, standalone compact/count, WebSocket lanes/steering, hosted tools, physical storage sharing and new resource profiles still need their own named evidence.

The b1 Worker run `40191955285` built Wasm and passed 363 library cases, while three leaf defects caused its integration job to fail. The 55/60 aggregate includes duplicate parent failures; it is not a passing candidate. Its known passing component/selector evidence can be sealed, and only the actual unresolved leaves and new changes need execution. Never replay those known passing 363 cases or 51-plus journeys merely to obtain a globally green label.

A coherent compiled API increment may be staged to learn its genuine native usage and new API behavior before hosted dispatch is complete, provided unavailable capabilities reject honestly before paid work. Such staging does not complete the full objective. Final release requires the aggregate named capability receipts, financial/resource closure, the exact public artifact gate and root's explicit decision. Do not inspect legacy production profiles/data or apply old release drafts before that clearance.
