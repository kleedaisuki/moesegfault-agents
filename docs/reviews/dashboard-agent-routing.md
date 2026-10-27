# Dashboard agent routing review

## Scope

Independent review of the agent model-transport preservation fix and the
progressively disclosed image/search service editor. The review follows the
working-tree change based on `366aee2`, not a deployed or accepted release.

Reviewed `apps/dashboard/src/{Agents.tsx,api.ts,ToolServicesEditor.tsx,
AgentRouting.test.tsx}`, both locale additions and the new
`e2e/agent-routing.spec.ts`. Actual serialization and credential-family contracts
were checked against `agents-core/src/{agent.rs,tool_services.rs}` and the model
normalization/provider-resolution callers.

## Findings

### P2: selecting a native platform preset loses its registry transport

The platform model registry supplies `ModelConfig.protocol` for the new direct
DeepSeek presets. The dashboard `Model` type does not expose this field, and
`AgentEditor` initially uses an empty transport state for a new agent. Neither
`changeProvider` nor `changeModel` adopts the selected preset's transport.
Submission therefore omits `model.protocol` even when the selected registry
entry explicitly specifies `responses`.

This is observable behavior, not just metadata loss: `normalize_model` copies
registered budgets but deliberately preserves the requested transport, while
`provider_config::effective_protocol` resolves omission to the OpenAI-compatible
credential family's legacy Chat Completions protocol. A user selecting the new
native DeepSeek preset consequently gets the wrong transport and cannot use its
native count/context capabilities without discovering and changing the advanced
transport control.

Recommended correction: expose the optional registry protocol and use its
default on new model/provider selection, while preserving the exact omitted or
explicit transport of an existing agent during an unrelated edit. An explicit
user transport override must remain distinguishable from an untouched default.
Add a focused fixture with a registry entry containing `protocol: "responses"`.

Status: source-fixed and independently re-reviewed. The registry `Model` type now
includes the optional protocol. Explicit user model/provider selection can apply
that default, but initial hydration does not reinterpret an existing omission.
An existing or manually selected explicit transport remains authoritative.

### P2: rebuilding an untouched nested model can discard newer server fields

The form still constructs `PATCH.model` field by field, even during a name-only
edit. A newer backend can therefore return a model field that this dashboard
bundle does not yet recognize, and the unchanged edit removes it when the backend
replaces the nested model. This is the same rolling-version compatibility failure
mode as the original missing-protocol defect.

The captain selected domain-level omission rather than a field-preservation
blacklist. The implementation now tracks explicit model edits and omits
`PATCH.model` altogether for unrelated edits. Model validation/building and
loading gates apply only when that domain is edited; new agents always include
their model. Provider, key, model, transport and both budget controls mark the
domain edited. The saved-key HTML requirement is likewise conditional, so a
name-only edit does not wait for or reinterpret key metadata.

Status: source-fixed and independently verified. A runtime fixture includes a
future nested field and leaves key metadata unresolved. Its name-only PATCH
omits `model`, allowing the backend's existing top-level merge to preserve the
whole domain. The targeted reviewer run passed **1 case, 15 skipped**. The owner
also observed the intended negative control before fixing the unconditional
HTML `required` attribute; assertions were not weakened.

## Search budget preservation

Changing only the search provider currently reconstructs its object with the
provider field alone. This clears the previous `max_tool_calls` and
`max_output_tokens`, as well as incompatible model/key fields. The core schema
applies these numerical budgets independently of search provider. This was
corrected: route selection removes incompatible model/key/provider fields while
retaining both numerical bounds and other untouched properties. The focused
fixture checks `max_tool_calls: 2` and `max_output_tokens: 2048`. Resetting image
credentials/model/asset hosts is intentional and is not reported as a defect.

## Verified boundaries

- An ordinary existing-agent edit retains explicit `responses`, `openai` and
  `anthropic` values; omission remains omitted.
- Untouched `tool_services` is omitted from PATCH entirely. Editing one service
  retains the other service object, including its limits and image asset hosts.
- Image/search credentials remain independent of the language-model credential.
  Anthropic-family credentials are not offered for OpenAI-family services.
- The transport control does not invent a Responses credential family. An
  explicit incompatible transport is rejected before the HTTP mutation.
- Service fields use the actual core names, limits and provider enums. UI
  controls reuse the existing collapsed-details, form-row and modal styling;
  both new English and Simplified Chinese labels are present.

## Verification boundary

The reviewer independently executed only the four new/changed preset,
explicit-override, omitted-hydration and search-switch cases: **4 passed,
11 skipped**, using the local Vitest entrypoint and a focused name pattern.
The implementation owner separately reports 15 focused unit cases, TypeScript,
and two local-browser journeys passing. The reviewer inspected both screenshots:
English/light at 320 px and Simplified Chinese/dark at 390 px, with no observed
horizontal overflow or theme/layout defect. Screenshot receipts are under
`.cache/dashboard-e2e/agent-routing-*/routing-mobile.png`.

The final unknown-field/slow-metadata correction has its own focused positive
check above. The earlier four-case run preceded the domain-omission refinement;
it is not represented as an execution of the final changed assertions. The owner
is responsible for the final affected-test/TypeScript receipts and the captain
for exact-source CI integration. Previously passing unrelated suites need not
be repeated.
No old 91-test dashboard suite, stream benchmark, paid model request, deployment,
Git index operation or production edit was performed by the reviewer.

## Conclusion

No substantive unresolved review finding remains in this scoped change.
Both identified compatibility problems and the search-budget preservation issue
have been corrected. This is source and focused local verification closure,
not a staging or full-product acceptance claim.
