# Interpreter allocation profiles and kernel verification

## Physical allocation is a contract

The admitted Responses Code Interpreter memory tier must match actual physical
Container allocation. This is not established by a model-facing label, Python
`resource` limits, an environment variable or available host RAM. Supported tiers
are currently `1g` and `4g`; `16g` and `64g` fail admission before model/compute
reservations. An existing running Container must never be resized to satisfy a
later tool declaration.

## Current Cloudflare constraint

Verified official documentation on 2026-10-25:

- Under the existing `default` scheduling policy, Wrangler fixes the application
  instance type. `basic` provides 1 GiB; `standard-1` provides 4 GiB.
- Runtime `ctx.container.start({instance: ...})` selection is available only under
  the `durable_object` policy, currently public beta. The policy cannot change in
  place: moving an existing application replaces its running instances.
- The runtime named-size list explicitly excludes `basic`. Omitting a size starts
  `lite` (256 MiB). Custom runtime sizes require at least one vCPU and a minimum
  of 3 GiB per vCPU, so a custom 1 GiB profile is not an alternative.

Consequently adding `instance: "standard-1"` to the existing `default` startup
dictionary is not a supported two-profile implementation. Migrating the whole
application to the beta policy would also not preserve its 1 GiB profile or
running workspace leases.

Primary sources:
[limits and instance types](https://developers.cloudflare.com/containers/platform/limits/),
[scheduling policy and runtime selection](https://developers.cloudflare.com/containers/configuration/scheduling-policy/),
[lifecycle and rollout behavior](https://developers.cloudflare.com/containers/concepts/architecture/).

## Recommended compatibility-preserving integration

Keep the existing Session application and namespace on `basic`. Add a 4 GiB
`standard-1` application with a separate Durable Object export/namespace which
delegates to the **same Rust Session implementation**, not a second harness.
Each session still has one actor and one physical Container. Store an immutable
allocation/profile routing choice with new-session admission; absence means the
legacy basic profile. Select a 4 GiB session for a new default-memory Code
Interpreter request. A continuation cannot relabel a live basic actor as 4 GiB;
an explicitly independent fork may use the established owner-bound branch and
workspace transfer protocol.

This requires approval and coordinated edits to namespace routing, session
storage, plan/resource admission, Wrangler and billing profiles. Those shared
changes are **not implemented by the daemon guard**. Do not deploy a global
application resize as a shortcut. A future deliberate beta-policy migration can
be evaluated independently with checkpoint/drain and rollback acceptance.

## Daemon guard

`interpreter_limits.rs` reads the kernel's cgroup membership and mountinfo,
locates the memory controller, and takes the minimum visible ancestor limit.
It supports cgroup v2 `memory.max` and the v1 memory-controller file. Missing,
unlimited, malformed or nonmatching allocation does not authorize an explicitly
requested tier. Mount-root offsets and cgroup namespace roots are handled without
joining parent components outside the visible hierarchy.

The private `POST /v1/python` request gains optional `memory_limit: "1g" | "4g"`.
When present, the guard runs before workspace-gate acquisition, execution-ID
consumption or kernel creation; failure returns HTTP 422. A rejected ID can be
retried with the actual admitted tier because no execution was accepted.

Legacy private daemon requests without this field retain their existing behavior
and make no memory-allocation claim. New public Responses bindings must always
send the explicitly admitted tier. The guard checks reality; it neither allocates
memory nor changes the Container profile or Python process limits.

Kernel reference:
[cgroup v2 memory controller](https://docs.kernel.org/admin-guide/cgroup-v2.html#memory),
[proc_pid_mountinfo](https://man7.org/linux/man-pages/man5/proc_pid_mountinfo.5.html).

## Focused acceptance

Four Rust tests cover exact tier equality, unlimited/unknown allocation,
hierarchical minima, namespace/mount-root mapping, v1 controller selection,
invalid paths and unsupported native tiers. They require the next GHA compilation.

`tests/e2e/test_workspace_interpreter_memory.py` creates separate actual Docker
instances with `--memory=1g`, `--memory=4g`, and no explicit limit. It checks wrong
tier rejection before execution-ID acceptance, then executes one cell under the
matching real allocation. The unrestricted private legacy case does not claim
to implement either tier. These are new focused journeys; the global passing
daemon fixture is not changed. Three cases were collected locally; Docker and
Rust execution remain GHA acceptance, not a claimed local result.
