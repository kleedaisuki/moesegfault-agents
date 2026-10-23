# Account skills registry

Skills are named instruction text, not executable tools or client-provided tool
schemas. Only the server's harness/workspace registry can define tools (`time`
and `bash`). A skill can describe how to use those tools but cannot grant a
workspace, credentials, or additional capabilities.

## Management API

All endpoints require a signed-in human account (browser session with existing
CSRF protection, or native CLI OIDC access token). Agent API keys cannot read or
mutate this control-plane registry. Agent configurations can reference their own
account's skill names through the existing `skills` array.

| Method | Path | Body / result |
| --- | --- | --- |
| GET | `/v1/skills` | `{object:"list",data:[skill,...]}` |
| POST | `/v1/skills` | `{name,instructions}`; returns skill, 201 |
| GET | `/v1/skills/{name}` | Skill including full text |
| PUT | `/v1/skills/{name}` | `{instructions}`; returns skill, 200 |
| DELETE | `/v1/skills/{name}` | Deletes account entry and version history, 204 |

A skill response is `{name,version,instructions,source}`. `source` is `account`
or `deployment`; `version` is server-generated. Bodies reject unknown properties,
including `version`, `tools`, and `account_id`. Names are 1–64 lowercase ASCII
letters, digits, underscores or hyphens. Instructions must contain non-whitespace
text and at most 49,152 UTF-8 bytes. The complete encoded JSON body is limited to
65,536 bytes, including escape expansion. Each account may own at most 50 names;
the database enforces this atomically even under concurrent creation. Updating
an existing name does not consume another name slot.

Optional deployment defaults come from `SKILLS_REGISTRY_JSON`, the existing
array of `{name,version,instructions}`. These are visible and selectable but not
mutable through account APIs. An account can create a same-name personal entry
to override the deployment default; deleting it reveals the default again. PUT
and DELETE on a deployment-only entry return 404. Registry listing merges both
sources deterministically by name. Defaults cannot contain duplicate names.

## Versioning, caching, and ownership

`0006_skills_registry.sql` separates mutable account heads (`skills`) from
immutable content versions (`skill_versions`). A version is the lowercase
SHA-256 digest of `magents.skill.v1\0`, the name, `\0`, and exact instruction
UTF-8 bytes. Reverting to identical content reuses its version. Changing whitespace
is a real content change. D1 transactional batches create version/head together;
quota errors and conflicting creation do not leak orphan versions.
This uses [D1's transactional batch contract](https://developers.cloudflare.com/d1/worker-api/d1-database/#batch),
not a Worker-side transaction spanning awaits.

All database access binds the authenticated account ID. Composite foreign keys
prevent even accidental cross-account head references. Version records cannot
be updated. Deletion removes account heads/history; account erasure cascades
through both. History is user data, not telemetry or a temporary cache; it is
never included in diagnostic logs.

At **new session creation**, the runtime resolves the account's requested names,
overriding deployment defaults, and copies each complete
`RegisteredSkill {name,version,instructions}` into the session's `PinnedContext`.
The prefix is sorted and persisted with the session. Updating/deleting a skill,
changing an agent configuration, or deploying changed defaults never rewrites
an existing session's instructions or cache prefix. Existing sessions therefore
remain usable even after deletion; create a new session to adopt new content.
Unknown names fail validation/session creation instead of silently disappearing.
Empty skill selections avoid the registry D1 read entirely. Named selections use
one indexed bounded account query, not one round trip per skill.
The query binds a JSON name array through SQLite `json_each` and fetches only
selected instructions, not every account skill's potentially large text.

## Verification

`tests/test_skills_sql.py` extracts the production SQL statements and exercises
create/list/update/delete on the migrated SQLite schema, account isolation,
cross-account reference rejection, quota rollback, immutable versions, concurrent
delete/update ordering, account erasure, UTF-8 bounds, and randomized version
revisits. Worker tests check input/schema injection and digest determinism.
Run `python -m unittest discover -s tests -p 'test_*sql.py' -v`; the existing CI
discovers the registry tests automatically. Worker unit/compile checks run in CI.
The `agents-core` property test
`personal_skill_pins_survive_registry_edits_and_removal` generates arbitrary
instruction replacements and checks actual persisted prefix bytes/hash stability.
