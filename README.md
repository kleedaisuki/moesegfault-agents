# moeSegFault Agents

Rust agent sessions on Cloudflare Workers, Durable Objects and on-demand container workspaces. OpenAI-compatible clients call an agent configuration as a model; `magents` and the React dashboard manage the same account and sessions.

| Surface | Production | Staging |
| --- | --- | --- |
| API | `https://agents.moesegfault.dev/v1` | `https://agents-staging.moesegfault.dev/v1` |
| Dashboard | `https://adash.moesegfault.dev` | `https://adash-staging.moesegfault.dev` |

## Versions and availability

Version **0.3.0** adds Responses, Space and hosted tools while retaining Chat
Completions and established management contracts. A v0.2 deployment supports the
Chat API, not Responses. Publication and deployment are separate: check the
[published releases](https://github.com/kleedaisuki/moesegfault-agents/releases)
and the selected host's `GET /healthz` build before choosing a client workflow.
An Actions artifact or enabled staging feature is not a production release.

| In v0.3 | Start here |
| --- | --- |
| Responses: streaming, background runs, observation/replay and branches | [HTTP API](docs/api.md#responses) |
| Conversations, exact input counting and compaction with protected instructions | [Context and state](docs/full-responses-auxiliary.md) |
| Local tools, code execution, image generation, web/file search and MCP | [Tool boundaries](docs/full-responses-tools.md) |
| One account Space, uploads and immutable workspace versions | [Space and workspace](docs/api.md#files-account-space-and-workspace-versions) |
| Dashboard languages/themes/mobile layout, Rust CLI and portable agent skills | [CLI](docs/cli.md) · [Skills](skills/README.md) |

Read [v0.3 notes](docs/releases/v0.3.0.md) for the user-facing changes and
[release readiness](docs/v0.3-release-audit.md) for the actual promotion gates.

For another agent to use the CLI, integrate the API, or contribute to the
platform, choose the corresponding [portable skill pack](skills/README.md).
Each loads task-specific references on demand rather than the full manual.

```mermaid
flowchart LR
    SDK[OpenAI SDK] --> API[Rust Worker]
    CLI[magents] --> API
    Web[React dashboard / BFF] --> API
    API --> Identity[moeSegFault Identity]
    API --> Billing[moeSegFault Billing]
    API --> D1[Accounts / config / ledger: D1]
    API --> Session[One Rust Durable Object per session]
    Session --> Provider[OpenRouter / DeepSeek / BYOK]
    Session --> Container[On-demand Rust workspace daemon + bash]
    Container --> Checkpoint[Batch compressed checkpoint]
    Session --> R2[User conversations and checkpoints: R2]
    Checkpoint --> R2
```

## Call an agent

Create an account API key and an agent configuration first. The SDK `model` is the **agent ID**, not the upstream provider model. Management uses cookie-bound BFF authentication or a registered native client's bearer credential; do not put an OAuth bearer token in browser storage.

For a v0.3 host, use the ordinary Responses SDK:

```python
import os
from openai import OpenAI
from openai.types.responses import Response

client: OpenAI = OpenAI(
    base_url="https://agents.moesegfault.dev/v1",
    api_key=os.environ["MAGENTS_API_KEY"],
    max_retries=0,
)
# The application chooses and durably records this key before submitting work.
request_key: str = os.environ["MAGENTS_REQUEST_KEY"]
response: Response = client.responses.create(
    model=os.environ["MAGENTS_AGENT_ID"],
    input="What time is it?",
    extra_headers={"Idempotency-Key": request_key},
)
print(response.output_text)
```

Persist the returned response ID. Continue with `previous_response_id` and only
new input; a genuinely new request gets its own idempotency key. Observe a known
response after disconnecting rather than recreating it. If submission itself was
ambiguous, retain the **original** key: a new key can start and charge new work.
See [Responses](docs/api.md#responses) for streaming, local tool handoff and recovery.

Chat Completions remains available. Its continuation uses the returned session
header and only newly appended messages; see the [Chat SDK journey](docs/api.md#openai-sdk-journey).
The service implements its documented agent contract, not every unrelated OpenAI
product. Model/provider capability errors are explicit; unsupported controls are
not silently ignored.

## Download magents

Download the archive and matching `.sha256` for your chosen [published release](https://github.com/kleedaisuki/moesegfault-agents/releases). Maintainers can also run [Package magents](https://github.com/kleedaisuki/moesegfault-agents/actions/workflows/release-magents.yml) manually for 30-day Actions artifacts; these are separate from public release assets.

| Platform | Archive |
| --- | --- |
| Linux x86_64, glibc 2.35+ (Ubuntu 22.04+) | `magents-VERSION-x86_64-unknown-linux-gnu.tar.gz` |
| Windows x86_64 | `magents-VERSION-x86_64-pc-windows-msvc.zip` |

For Actions downloads, unzip the artifact wrapper first. Verify the inner archive before extracting it:

```sh
# Linux; run in the directory containing the archive and its .sha256 file.
sha256sum --check magents-*.tar.gz.sha256
tar -xzf magents-VERSION-x86_64-unknown-linux-gnu.tar.gz
./magents --version
./magents auth login
```

```powershell
# Windows; replace VERSION with the version you downloaded.
$archive = 'magents-VERSION-x86_64-pc-windows-msvc.zip'
$expected = ((Get-Content "$archive.sha256" -Raw).Trim() -split '\s+')[0]
if ((Get-FileHash $archive -Algorithm SHA256).Hash.ToLowerInvariant() -ne $expected) { throw 'Checksum mismatch' }
Expand-Archive -LiteralPath $archive -DestinationPath magents
.\magents\magents.exe --version
.\magents\magents.exe auth login
```

Move the verified binary to a directory on your `PATH` if desired. Interactive login uses Windows Credential Manager or Linux Secret Service; Linux desktop users need a running, unlocked Secret Service implementation such as GNOME Keyring or KWallet and a session D-Bus. There is no plaintext credential fallback. Headless environments can supply `MAGENTS_API_KEY` for API-key-authorized operations; account/skill/provider-key management still requires human login. macOS binary distribution is not included yet; build from source with `cargo install --locked --path crates/magents` when Rust and the platform keychain are available.

Each archive includes the license and `BUILD-INFO.json` with the actual Cargo version, source commit, target, and workflow run. Checksum sidecars use LF line endings on every platform. Checksums detect corruption, not publisher authenticity; these initial binaries are not code-signed. Manual runs default to artifacts only. To recover asset publication for an existing release, explicitly set the workflow's optional `release_tag` input (for example, `v0.1.0`): the workflow verifies the release version, actual binary source, both native platform receipts and checksums before attaching assets. This does not move the tag, create a release, or overwrite assets. A published release must match the package version (`vVERSION`); build metadata must retain the actual binary source even when verified artifacts are promoted. A release page without assets is not a completed binary publication.

## Invariants

- Instructions, resolved tool schemas and skills are pinned when a session is created. Configuration changes affect new sessions, not existing cache prefixes.
- Context compaction is a validated percentage of the selected model's context budget. Compaction does not overwrite the original conversation journal.
- Reads, model output and tool activity do not change the last-user-append clock. A running task continues after ten minutes without new input; it archives and releases its workspace after completion. Idle sessions archive at ten minutes.
- Harness tools and workspace tools have separate execution boundaries. Bash never receives provider keys, OAuth tokens, R2 credentials or the supervisor's authentication secret.
- Checkpoint upload and durable pointer commit precede container destruction. A failed checkpoint fails visibly rather than pretending data was saved.
- Free accounts have explicit limits. Model usage and infrastructure usage use an idempotent monetary ledger and versioned tariffs, not sampling-based telemetry.
- User data, intermediate execution state, monetary records and opt-in diagnostics have different storage and retention policies.

## Repository

| Path | Responsibility |
| --- | --- |
| `crates/agents-core` | Platform-independent state, pinned context, configuration and quotas |
| `crates/agents-auth` | Identity validation, encryption and exact monetary domain types |
| `crates/agents-worker` | Worker API, Rust session/auth Durable Objects, providers and bindings |
| `crates/workspace-daemon` | Credential-isolated bash supervisor, checkpoint and restore |
| `crates/magents` | Rust management CLI and system-browser login |
| `apps/dashboard` | Same-origin React management UI |
| `migrations` | Agents D1 schema and accounting constraints |
| `tests/e2e` | Actual SDK and container HTTP user journeys |

Read [architecture](docs/architecture.md), [workspace](docs/workspace.md), [observability](docs/observability.md), and [deployment](docs/deployment.md). Tests and optimized builds run on GitHub Actions; local work is limited to focused checks and user journeys when useful.

## Development

```sh
npm ci
npm --prefix apps/dashboard ci
npm --prefix apps/dashboard run build
cargo test --locked -p agents-core -p agents-auth -p magents
cargo install --locked worker-build --version 0.8.7
npm run build:worker
```

`wrangler dev --env staging` also requires Docker for container work and a deliberately configured local Identity client. Never reuse production login callbacks or secrets for local development. The daemon must run in its dedicated container; do not run its privileged supervisor directly on a shared host.

License: GPL-3.0-only; see [LICENSE](LICENSE).
