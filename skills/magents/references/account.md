# Account access and BYOK

All examples use production. For staging, prepend `--api-url https://agents-staging.moesegfault.dev` immediately after `magents` on every invocation. `MAGENTS_API_URL` can otherwise change the origin; check it without dumping the environment.

## If the CLI is missing

Check `magents --version` and `--help`; TOML publication and data exports require
v0.2 or newer. When obtaining the tool is in scope, use the matching archive and
checksum from the [official releases](https://github.com/kleedaisuki/moesegfault-agents/releases),
verify SHA-256 before running it, and follow the platform's credential-store
prerequisites. Published binaries cover Windows/Linux x86_64; consult the official
source-build instructions for other targets instead of inventing a package-manager
entry or macOS archive. Do not replace a user's existing installation without
their intent, or compile the entire platform merely to use the client.

## Choose the credential, not just the command

| Credential | Intended use | Handling |
| --- | --- | --- |
| Human OIDC login | Account management, personalized skills, provider-key management | Browser Authorization Code + PKCE; tokens in the OS credential store |
| Platform API key | Authorized automation calling this platform | Secret-manager injection as `MAGENTS_API_KEY`; never a CLI argument |
| BYOK provider key | Authenticate requests to a selected upstream endpoint | Human enters it at the CLI's hidden terminal prompt; agent TOML stores only its credential ID |

`MAGENTS_API_KEY` overrides saved interactive credentials. For personalized-skill or BYOK management, remove that override from the command environment and use the human login. Do not print or ask the user to paste any secret into chat.

```sh
magents auth login
magents auth status
magents account
magents keys list
magents usage
```

Have the human complete login in a browser they control. `auth login --no-browser` prints the authorization URL instead of opening a browser, but still needs the exact CLI loopback callback within three minutes; it is not device-code login. On SSH, arrange an appropriate loopback port-forward rather than inventing a callback. `auth status` checks local expiry, not current server authorization. `auth logout` clears only the local login, not Identity SSO, provider grants, or an environment API key.

For explicitly requested automation-key issuance, `magents keys create NAME` returns the secret once. Arrange protected storage before running it; do not expose its output in chat or routine tool logs. Revoke a selected key with `magents keys delete KEY_ID` only when requested.

## Configure a custom endpoint

```sh
magents byok list
magents byok add openai --base-url https://api.example.com/v1 --name research
magents byok add anthropic --base-url https://api.anthropic.com --name claude
magents byok set CREDENTIAL_ID
```

The `add` and `set` commands require human hidden-key entry. Do not script key input through flags, chat, or environment dumps. Use `byok add --help` / `set --help` for installed-version options.

The protocols are `openai` (alias `openai-compatible`) and `anthropic`, not a list of vendor names. OpenAI-compatible routes append `/chat/completions` to the supplied base; include `/v1` when required. Anthropic normalizes a missing `/v1`, then appends `/messages`. Endpoints must be public HTTPS hostnames without userinfo, queries, or fragments; IP literals and local/internal names are rejected, and redirects are not followed.

An existing credential's protocol and normalized endpoint are immutable. `set ID` rotates the key, optionally renaming it; add a new credential to change route, then update the agent. Previously pinned sessions retain their credential reference. `byok delete ID` removes an owned credential and can affect its consumers; do not use deletion as a connectivity test.

Use `provider = "byok"`, returned `credential_id`, endpoint model ID, and supported budgets in [agent configuration](configuration.md). BYOK avoids platform model charges, not separately metered workspace/storage costs or upstream charges.

`magents diagnostics` is an offline minimal preview. Upload is a separate explicitly authorized action requiring both `--upload --consent`; do not add it routinely to troubleshooting.

## Plan and subscription management

Use `account` and `usage` to inspect verified plan, limits and metering. For a
requested subscription change, follow the account response's `subscription_url`
or the dashboard's Usage & plan button to Subscribe; there is no invented
`magents subscribe` checkout command. The fixed return refreshes verified grants.
Do not infer a paid plan from URL parameters, issue administrator grants, enter
activation capabilities into URLs, or approve a purchase merely to clear a quota
error. Subscription and payment actions must match the user's actual request.

Full contracts: [CLI authentication and management](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/cli.md), [BYOK protocols](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/byok-protocols.md).
