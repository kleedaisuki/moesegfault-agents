# Stable source rendering during live output

## Workload and bottleneck

The existing incremental HTTP pager already avoids transferring the entire source
journal on every poll. This change addresses a different cost: `Conversation`
previously mapped and rendered every cached source message whenever `partial`
changed. Those source entries are immutable; only the separate live preview changes.
Typing into the composer caused the same unrelated history work.

The bounded workload uses the actual `Conversation` component, normal QueryClient
cache and real `MessageContent` renderer. It represents two loaded pages: 100
source entries with 4 KiB tool text each, followed by 24 independently flushed
stream deltas. The instrumentation counts original history renderer calls and
React Profiler `actualDuration`; it does not replace the renderer with fake work.
The original component from `6552a93` fails the new zero-repeat contract with
exactly **2,400** redundant history renders, demonstrating test sensitivity.

## Implementation

`JournalMessages` owns rendering of the stable source array. Its memo boundary
does not reevaluate that array for unrelated composer/live-preview changes.
Memoized rows additionally retain unchanged message references when an append or
older page changes the array. The existing source sequence remains the React key.
Language changes still update localized role labels through the history component's
translation subscription; typed content keeps its existing renderer and safety rules.

There is no new deep comparator, copy of source text, historical cache, request,
debounce or frame timer. The preview's update cadence and first visible delta are
unchanged. The DOM structure, source ordering, full history and explicit older-page
behavior are also unchanged. React retains the latest memo props; extra React
component boundaries have a small unmeasured memory cost, not a new data cache.

This follows React's [reference-based memoization contract](https://react.dev/reference/react/memo)
and [Profiler measurement semantics](https://react.dev/reference/react/Profiler).
The underlying incremental-computation principle is to reuse stable identified
work rather than recompute unrelated results; [Incremental Computation with Names](https://arxiv.org/abs/1503.07792)
provides a formal treatment of that principle. This change uses ordinary React,
not a new incremental runtime or a claim about the paper's speedups.

## Comparable measurements

Three alternating A/B pairs ran the same fixture on Windows x86-64, Node 26.10.0,
the project's React 19.3.1 development renderer and Vitest/jsdom. Each baseline run
temporarily used the exact old component source; the script restores the optimized
source in `finally`. The fixture, query data, actual renderer and input cadence
were identical. A baseline run must fail at the intended 2,400-versus-zero oracle;
an unrelated setup failure is rejected by the profiling script.

| Quantity | Original source | Stable history boundary |
| --- | ---: | ---: |
| History renderer calls during 24 deltas, every sample | 2,400 | 0 |
| Parent commits during those deltas | 24 | 24 |
| React update CPU, samples (ms) | 89.188 / 93.720 / 95.643 | 30.473 / 27.282 / 27.631 |
| React update CPU, median (ms) | 93.720 | 27.631 |
| Initial history render CPU, samples (ms) | 25.508 / 30.508 / 24.074 | 25.789 / 27.774 / 21.774 |
| Original execution POSTs | 1 | 1 |

The measured update median fell 70.5% in this instrumented workload. An earlier
alternating three-pair run measured 71.638 → 18.179 ms (74.6%); the host-load
variation is why the timing is descriptive, not a CI pass threshold. Initial
render samples overlap; this is not evidence of a statistically established
initial-render improvement or regression. Three pairs do not establish tails.
These are development React CPU observations, **not browser paint, mobile device
latency, production throughput, network savings or a deployment SLO**. The robust
regression invariant is elimination of unrelated source renders, not a timing limit.

Evidence: `.cache/perf/clients/paired-20261025T104306450/paired.json`, six per-run
JSON/log receipts, the earlier `paired-20261025T103717612` run and actual-source
negative control. Reproduce after
normal dashboard dependencies are installed:

```powershell
# Optional local A/B profiling only; edits just the owned component and restores it.
& .temp/perf-clients/profile-conversation.ps1

# Ordinary deterministic regression, included by the existing dashboard CI job.
npm --prefix apps/dashboard run test -- src/Conversation.performance.test.tsx src/JournalMessages.test.tsx
```

The profiling script is a task-local diagnostic, not required production tooling.
`Conversation.performance.test.tsx` emits a safe aggregate console receipt; the
profiling script saves it to `.cache/perf/clients/conversation-render.json`.
The regular test checks the real
Conversation path, exact visible source and live text, one POST, and no repeated
history work. Additional tests cover prepend/append/replacement and unchanged-entry
language switching. Full final-source Actions/browser acceptance remains the
delivery owner's gate; local fixture measurements do not replace it.

## Integrated checkpoint

The optimization was committed as `bacb866`. The integrated performance checkpoint
at `1c2620161419b6ad7f6c1aeb55db689315202c59` passed
[GitHub Actions run 40186260916](https://github.com/kleedaisuki/moesegfault-agents/actions/runs/40186260916),
including the dashboard's 91 tests, production build and browser journeys.
The development A/B measurements above remain a source-rendering CPU result, not
a deployed latency or mobile-device SLO. Later additive API compatibility changes
have their own focused acceptance; this checkpoint is not evidence for those.
