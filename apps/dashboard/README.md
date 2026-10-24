# Dashboard

React management client for `adash.moesegfault.dev`. The Rust Worker serves the
built assets and the same-origin BFF; the browser never calls Identity or Billing
with OAuth tokens. API keys shown once after creation and provider key drafts live
only in component memory. All configuration and session data comes from real API
responses; there is no production demo fallback.

## Develop and verify

```sh
npm ci
npm run dev
npm test
npm run build
npx playwright install chromium
npm run test:browser
```

Run these commands in `apps/dashboard`. Vite proxies `/v1` and `/auth` to the
local Worker at `127.0.0.1:8787`. Deploy `dist/` through the Worker assets binding,
not a different-origin API URL. Node 24+ is recommended (Vite 8 needs modern Node).
The package lock is independent from the root deployment tools lock.
The optional non-secret build variable `VITE_AGENTS_API_ORIGIN` sets the public
SDK endpoint shown in the collapsed API sample. By default matching
`adash[-staging]` hostnames map to `agents[-staging]`; local and preview origins
remain same-origin. This setting does not change the browser BFF origin.

Browser tests use explicit HTTP contract fixtures, not a live Identity account.
They test keyboard interaction, native dialog behavior, agent creation, CSRF, and
mobile overflow. Vitest journeys cover authentication failure, configuration
creation/failure, session steering, diagnostics consent, and API error handling.
Neither substitutes for the staging Identity → agent → persistent workspace
acceptance gate. `PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH` optionally reuses an
installed browser locally. Test output and screenshots stay in root `.cache/`.

## Boundaries

- `api.ts` is the wire contract and cookie-only transport. Account query provides
  an in-memory CSRF token; mutations send `X-CSRF-Token`. Expired authentication
  renders a sign-in action. No OAuth tokens or secrets enter browser storage.
- TanStack Query owns server state and request deduplication. Session polling
  stops on closure; switching away unmounts the observer. Polling is not user
  activity and must never advance the server idle clock.
- Agent model budgets come from `/v1/models.available_models`; model limits are
  not editable or guessed. Instructions, tools, and skills are pinned by the
  server for each session. Editing a configuration affects new sessions only.
- Session messages start an idle run or steer a running run. Archived sessions
  explicitly resume through admission before accepting messages.
- Diagnostics show the exact allowlisted JSON, require fresh consent, and exclude
  logs, user-agent fingerprints, credentials, free-form notes, and conversations.
- Native `<dialog>` supplies modal focus isolation. The trigger is restored after
  unmount; React Strict Mode lifecycle replay must not lose it.
- Fonts are self-hosted package assets. There are no analytics or third-party
  font requests.

## Upstream references

React recommends cleanup or a server-state library for fetch effects:
https://react.dev/reference/react/useEffect#fetching-data-with-effects .
TanStack Query handles the dashboard's server-state cache instead of custom
fetch-effect state machines. Vite build/deployment contract:
https://vite.dev/guide/static-deploy.html .

## Language, appearance, and accessibility

English and Simplified Chinese catalogs are maintained together in `src/locales/`.
`react-i18next` updates visible labels, validation/error messages, accessible names,
roles and statuses when language changes. Agent/user text, model identifiers, key
prefixes, machine diagnostic payloads and the API code sample remain verbatim.
Unknown upstream errors receive a localized HTTP-class explanation in Chinese,
not a raw English fallback. A request ID remains available for support. Dates and
USD are formatted with the selected locale. Ledger decimal microUSD strings use
BigInt all the way through aggregation and currency formatting, including values
larger than JavaScript's safe integer range.

Only `magents.language` and `magents.theme` are stored in localStorage. The latter
accepts `system`, `light` or `dark`; invalid/unavailable storage safely falls back.
The small self-origin `public/appearance.js` runs before the stylesheet/module to
apply saved appearance without a light-theme flash, follows operating-system
changes in System mode, and updates browser chrome. It needs no inline-script CSP
exception. Secrets and conversation data are never stored there.

The shell exposes a skip link and landmark navigation. All interactive controls
have at least 44px height; icon controls have 44px width. Input typography stays
16px to prevent involuntary iOS zoom. The 320px layout keeps navigation,
preferences, sign-out, agent/skill forms, streaming conversation, API/provider
keys, diagnostics, and billing reachable. Theme palettes use semantic tokens and
higher-contrast secondary text. Native modal focus isolation, Escape, trigger
restoration, reduced-motion handling and an error boundary cover keyboard and
load-failure paths.

## Server state and response latency

General server queries stay fresh for 60 seconds; model/skill registries for five
minutes. Mutations invalidate only their affected keys. Personal skill CRUD uses
`/v1/skills`; deployment entries can be copied into account-owned overrides but
not deleted. Agent configuration selects registry names; the server resolves and
pins instructions/version per session. No registry polling or tool schemas are
sent by the browser.

Idle session runs consume actual SSE deltas using `eventsource-parser`; the
composer remains available for concurrent content-only steering. Polling pauses
while that stream is attached, and the final authoritative snapshot replaces the
transient display. A disconnect does not issue a backend cancel action. Reopening
a running session uses snapshots: 2 seconds running/queued, 5/15/30 seconds as idle
age increases, no hidden-tab/background polling, no polling after archive or
401/403/404, and bounded transient-error backoff. Snapshot GETs do not count as
user activity. The server owns the inactivity deadline and execution lifetime.

Skills, Access, and Usage panels load as separate chunks; fonts contain only the
Latin subsets actually used by the UI (system CJK fallback). Do not interpret
emitted font byte reductions as measured network savings: unicode-range already
lets browsers defer unused subsets.

`public/_headers` is copied into `dist/` and applies one-year immutable browser
caching **only** to Vite's content-hashed `/assets/*` files. HTML and the stable
`appearance.js` keep normal revalidation, and Worker-generated `/auth/*` and
`/v1/*` responses retain their private/no-store policy. Never put mutable or
user-generated files under this asset prefix. This uses the platform's supported
[_headers mechanism](https://developers.cloudflare.com/workers/static-assets/headers/),
not a second service worker or application cache. The rule works on the existing
asset-first routes; it does not override headers on Worker-generated responses.

The bounded thirteen-journey Chromium suite includes en/light desktop, zh/dark desktop,
zh/light 320px and en/dark 360px complete management journeys; all four plan names;
registry creation/selection; streaming then steering; one-time key display;
provider key storage; diagnostics consent; locale/theme persistence; live System
mode changes; touch target sizes; overflow; and native modal keyboard behavior.
The HTTP fixtures test the browser contract, not a deployed Cloudflare/Identity
acceptance flow. Unit tests cover catalog completeness, localized failures,
precision above 2^53, resource sums, polling policy, fragmented UTF-8 SSE deltas
before completion, truncated streams and structured stream errors.

## Public entry and authentication boundary

An account query decides which tree may mount. Session verification and network
failure have public loading/retry states; a 401 gets a dedicated product/sign-in
entry, never dashboard navigation, account placeholders or private resource
requests. A later management/stream 401 unmounts the authenticated tree, clears
CSRF and private query state, and offers sign-in for the selected page. Only the
allowlisted `#skills`, `#access`, and `#usage` destinations are forwarded to the
same-origin BFF; arbitrary fragments are not redirect targets. The dashboard
modules are lazy-loaded after authentication instead of shipping them eagerly
with the public entry.

The layout follows the separation and single-primary-action approach visible on
[Vercel's login page](https://vercel.com/login) and the workspace-first sign-in flow
documented by [Linear](https://linear.app/docs/login-methods): brand/preferences,
one account continuation action, and public documentation/CLI links. It does not
copy their authentication methods or invent demo account/workspace activity. The
terminal is a static CLI example, not a status readout. Four public-entry browser
cases verify English/Chinese, light/dark, desktop and 320/360px layouts, with
separate delayed-auth and expired-auth tests; actual deployed login remains a
separate end-to-end gate.

## Protocol-based BYOK

Access offers exactly OpenAI-compatible API and Anthropic, with a base URL and
secret API key plus an optional human-readable connection name. Creation uses
`POST /v1/provider-keys`, allowing multiple endpoints of the same protocol. The
list displays metadata only and deletion uses the opaque credential ID, including
legacy vendor-specific credentials. Existing routes are not edited in place;
adding a different endpoint creates a new connection, preserving session pins.

BYOK agents accept custom model IDs rather than requiring the platform registry.
Advanced model limits default to a 131,072-token context budget and 8,192 output
tokens, remain editable, and are preserved when editing an existing custom agent.
These are configurable budgets, not a claim that an arbitrary endpoint supports
them: use its advertised model limits. Compaction remains a percentage of that
context budget. Platform-funded choices still use the server's priced registry;
legacy credential namespaces can still offer matching known model suggestions.
No raw API key enters agent configuration, browser persistence or diagnostics.
