# Code-managed prompt references

The Responses `prompt: {id, version?, variables?}` field resolves an owner-scoped
platform template. It is not forwarded to an unrelated provider's account.
A registry reference never replaces the session's protected server/agent prefix.
Prompt instructions are response-scoped defaults; explicit request instructions
win. Rendered template messages precede that request's new input exactly once.
Conversation history is prepared separately and is not reinserted by the prompt.

OpenAI currently retains this compatibility field but is deprecating managed
prompt objects, with its `v1/prompts` shutdown scheduled for November 30, 2026.
This platform therefore manages templates as reviewed, versioned configuration,
not a new prompt-management CRUD subsystem.

Primary references:
- https://developers.openai.com/api/docs/guides/prompting/migrate-from-prompt-object
- https://developers.openai.com/api/reference/resources/responses/methods/create
- https://github.com/mitsuhiko/minijinja (MiniJinja 3 rendering, strict variables,
  execution fuel and bounded writer output)

## Operator registry

Set `PROMPT_REGISTRY_JSON` as a private Worker secret for private templates. A
plain variable is also supported for non-sensitive reviewed configuration. Never
commit private owner content into this public repository. The versioned format:

```json
{
  "format": 1,
  "prompts": [{
    "owner": "account-id",
    "id": "pmpt_support",
    "current": "2",
    "versions": {
      "2": {
        "variables": {"customer": "text", "photo": "image"},
        "instructions": "Help {{customer}} resolve their issue.",
        "input": [{
          "role": "user",
          "content": [
            {"type": "input_text", "text": "Customer: {{customer}}"},
            {"type": "variable", "name": "photo"}
          ]
        }]
      }
    }
  }]
}
```

Registry `variable` parts are configuration syntax, not public Responses input
items. They insert one validated public `input_text`, `input_image`, or
`input_file` part. Public variables accept strings or the matching typed part;
registry variable kinds are `text`, `image`, and `file`. A string inserted as a
whole content part becomes `input_text`. Images and files use whole variable
slots, not string interpolation. Their owner, URI and byte resolution follows
the same existing attachment pipeline as ordinary request input.

Text templates use MiniJinja without HTML escaping. Variable values are literal
values, never parsed a second time as templates. All declared variables are
required and extra values fail. A text variable with a cache-breakpoint part must
use a whole variable slot so interpolation cannot silently discard its cache
metadata. Missing values, wrong content types and invalid input are rejected
before inference. Only operator-authored templates execute; callers cannot
supply template source or enable loaders.

Published versions must remain immutable: append a version and move `current`,
rather than editing an existing version. The registry has a 1 MiB envelope,
256 owner/id entries, 64 versions per entry and at most 128 variables/messages/
parts per template. Text rendering has strict undefined-variable behavior,
100,000 instruction fuel, a writer-enforced 256 KiB output bound per text, and a
1 MiB combined rendered payload bound. No raw engine error, variable or template
content is used in external error diagnostics.

## Accepted-response ownership and replay

Canonical Session admission checks an existing Response before resolving the
registry. It retains the original raw request for idempotency comparisons and
persists a private `ResolvedPrompt` containing the selected version, rendered
instructions/input and their digest. Public `response.prompt.version` reports
the selected version even when the caller omitted it.

Retries reuse the accepted Response and do not look up a newly published current
version. This remains true after a cold actor restart or removal of the old
registry version. Existing responses, source messages and pinned prefix hashes
are not rewritten. Rendered input becomes ordinary accepted source input; its
history therefore survives normal compaction and forks. Request-scoped rendered
instructions use the same instruction preservation path as explicit instructions.
Deleting a Response clears both raw prompt variables and its private rendered
prompt. These are user data, not disposable telemetry.

Automatic steering successors are not fresh client prompt submissions. Their
trusted queued-successor context inherits the saved version and instructions,
while clearing already accepted template input. The original resolution digest
continues to identify the inherited template; user source input is not duplicated
or re-rendered against a new current version. Explicit client continuations with
a prompt remain intentional new applications of that prompt.
Unknown IDs, versions and another owner's IDs return the same `prompt_not_found`
response. No-prompt requests do not read the registry and acquire no new fields
in serialized Response state. The public compact/count schemas do not gain an
unsupported prompt field; accepted source/context already carries prior resolved
prompt content.

## Incremental verification

`prompt_templates::tests` exercises typed parts, literal substitution, version
selection, serialized replay, owner isolation, malformed variables/configuration,
raw request identity and bounded output, plus generated literal-variable cases.
The controlled compiled-Worker selector is:

`Responses prompt templates preserve version owner variables and replay identity`

It captures actual prepared provider messages, checks protected pinned
instructions, explicit/current versions and caller ownership, then replaces the
registry and cold-replays the original idempotency key with no new inference.
The fixture uses no external network or paid model calls. Syntax/format checks
are not a substitute for that compiled incremental journey.
