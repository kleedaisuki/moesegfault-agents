# Agent integration skills

Install only the task-specific folders your agent needs. Each is a portable
[Agent Skills](https://agentskills.io/specification) package; copy the complete
folder, including `references/` and optional client metadata, into your client's
configured skills directory. No global installation or cloud registration is
performed merely by cloning this repository.

| Skill | Use it for |
| --- | --- |
| [magents](magents/SKILL.md) | Terminal account access, agent publication, sessions and downloads |
| [moesegfault-agents-api](moesegfault-agents-api/SKILL.md) | Applications calling the existing SDK/HTTP service |
| [moesegfault-agents-development](moesegfault-agents-development/SKILL.md) | Implementation, testing and delivery of the platform repository |

Discovery uses each skill's `name` and `description`. Activation loads the small
`SKILL.md`; that entry routes directly to only the reference needed for the task.
There is no mandatory nested router, all-documents preload or executable wrapper
around the existing CLI/SDK. `agents/openai.yaml` is optional client UI metadata;
other compatible clients can use the same Markdown packages without it.

These filesystem skills teach an external agent to use or integrate the project.
The service's [account skills registry](../docs/skills.md) instead stores named
instruction text selected by an agent configuration and pinned into a cloud
session. Installing one of these folders does **not** upload it to that registry,
add tools, grant permissions or start model execution.

## Keep the packs useful

Validate frontmatter with the standard `skills-ref validate PATH` command or your
skill author's format validator. Check local links after copying a folder out of
the checkout. Verify command examples against the installed CLI and request
examples against the maintained API contract; validation must not silently turn
into a production or paid model test.

Test the decisions, not only the shape: an interrupted run must not be replayed,
a provider key must not be used as the platform key, an Anthropic upstream does
not change the public client protocol, and an export must not mutate user activity.
Keep those distinctions in the relevant entry/reference rather than adding the
entire platform manual to every skill. One-level routing follows the format's
own guidance; [recent navigation research](https://arxiv.org/abs/2607.17598) also
motivates testing retrieval behavior rather than assuming deeper trees help.
