# Dashboard agent routing

The agent editor follows the existing secret-free contracts in
[`agent.rs`](../crates/agents-core/src/agent.rs) and
[`tool_services.rs`](../crates/agents-core/src/tool_services.rs). It does not
create a second credential store or infer hosted-tool service models from the
language-model registry.

## Editing without changing routing

`ModelConfig.protocol` selects the model wire transport; a saved key's protocol
is its immutable authentication family. Responses uses the existing OpenAI
authentication family, not a new type of saved key. The collapsed **Model
transport** control keeps an omitted protocol omitted, preserves an explicit
transport on rename/instruction edits, and permits returning to the provider
default. Switching to an incompatible authentication family requires an explicit
compatible/default transport before saving; it never silently changes a key.
An unchanged model is omitted as a whole from PATCH, preserving even nested
fields returned by a newer Worker that this dashboard does not understand.
Only explicit model/provider/key/transport/limit edits build a validated model
replacement. Ordinary name, instruction and tool-service changes do not depend
on model-registry availability or saved-key metadata finishing its fetch.
For a new explicit registered-model selection, the registry's optional protocol
supplies the preset transport. Initial hydration of an existing agent is not a
selection. A saved explicit transport or a manual transport choice takes
precedence over subsequent presets, including an explicitly chosen default.

The **Tool services** disclosure separates image and search routes from the outer
language model. Opening it reuses the account's cached key metadata, with no
secrets returned to the editor. It does not fetch keys for an ordinary platform
agent until opened, and its query observer is disabled again when closed.

An untouched `tool_services` is omitted from the top-level PATCH. The backend's
existing top-level merge therefore preserves the whole stored service object,
including image asset hosts and search resource bounds. Editing one service
retains the other service object. Selecting the platform default removes that
explicit service binding; an explicit empty object resets both overrides.
Switching search routes preserves existing independent resource bounds while
clearing the incompatible credential/model pair. Switching image dialect resets
its route-specific credential, model and URL-host policy; editing fields within
the existing image route preserves its other fields.

Image BYOK supports OpenAI Images and OpenRouter Images. Search supports platform
DeepSeek, dedicated DeepSeek BYOK and a separately configured Responses search
service. Dedicated DeepSeek search never receives an invented language-model
name. Native Responses search alone exposes its output/call bounds. Image URL
host allowlists and native-search limits are nested disclosures; ordinary
configuration stays compact. Keys and models are independent for each service,
and no base URL or plaintext key is included in an agent PATCH.

These controls configure service routing, not a guarantee that a deployed service
is enabled or that a specific provider supports every model/tool option. Backend
admission remains authoritative. They do not automatically declare a hosted tool
in a Responses request or change the main model to an image/search provider.

## Focused acceptance

Only the additive routing workflows are selected, not the previously accepted
91-test dashboard checkpoint:

```sh
npm --prefix apps/dashboard run test -- src/AgentRouting.test.tsx
# Only the three changed pre-existing edit workflows, not the old full suite:
npm --prefix apps/dashboard run test -- src/Agents.test.tsx -t 'preserves an existing model while metadata loads|preserves an existing BYOK model while|preserves existing custom BYOK model limits'
cd apps/dashboard
npx tsc -b --pretty false
npx playwright test e2e/agent-routing.spec.ts
```

The component integration tests exercise real `Agents` forms and the normal
React Query client with fixture API responses. They cover protocol preservation
and explicit reset, independent credentials, incompatible key changes, locale,
service-default restoration, rolling-version model fields and preservation of
untouched service objects. Three previously accepted edit-workflow assertions
are updated to check the same retained resource through an omitted model PATCH,
rather than requiring a redundant reconstructed model in the HTTP request.

The two new browser journeys use fixture HTTP routes, not staging authentication
or provider/model calls. They exercise English/light at 320px and Chinese/dark at
390px, inspect the actual PATCH and CSRF header, assert one key-metadata read and
one PATCH, and check viewport/modal overflow. Screenshots use the existing
Playwright output directory under `.cache/dashboard-e2e/`.

Local verification on 2026-10-25 passed 19 selected workflows (16 additive and
three changed edit cases; nine other old cases skipped), the TypeScript build
check and both browser scenarios. The browser runner used installed Chrome via
`PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH`; no browser download, live staging call,
credential mutation or provider generation was needed. The previously required
saved-key select was a real blocking negative control for a delayed metadata
fetch: the new opaque-model rename test submitted no PATCH until the select's
`required` constraint was scoped to an edited model, matching the mutation gate.
The final source receipt and screenshot hashes are saved in
`.cache/dashboard-agent-routing/receipt.json`. Production bundling and the exact
committed-source incremental Actions receipt remain the release captain's gate.

The separate previously accepted streaming optimization and its exact integrated
Actions receipt remain documented in
[`dashboard-stream-rendering.md`](dashboard-stream-rendering.md).
