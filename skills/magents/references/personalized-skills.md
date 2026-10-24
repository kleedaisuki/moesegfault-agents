# Server-side personalized instructions

The cloud registry holds named instruction text, not portable filesystem Agent Skill packages. `skills create` consumes JSON `{name,instructions}`; it does not upload a `SKILL.md` directory, references, scripts, or assets. Do not imply this portable skill folder is automatically discovered by the cloud harness.

Use human OIDC login for all registry commands. A platform API key cannot read or mutate this registry; remove `MAGENTS_API_KEY` from the command environment without printing it, then use the protected account login. Keep production/staging selection consistent with that login.

```sh
magents skills list
magents skills show review-rust
magents skills create skill.json
magents skills update review-rust update.json
magents skills delete review-rust
```

Run only the operations the user requested. Creation file:

```json
{"name":"review-rust","instructions":"Check ownership and compatibility."}
```

Update file:

```json
{"instructions":"Check ownership, compatibility, and session pinning."}
```

Both commands accept `-` for JSON stdin. Installed `create --help` and `update --help` expose their payloads offline. Names are 1–64 lowercase ASCII letters, digits, underscores, or hyphens. Instructions are non-whitespace text of at most 49,152 UTF-8 bytes; complete JSON including escaping is limited to 65,536 bytes. Each account can own 50 names. Do not submit `version`, tools, account IDs, or extra fields; the server computes content versions.

List responses merge deployment defaults and account overrides with `source`. To customize a deployment-only name, create an account entry of the same name rather than updating the deployment entry. Deleting that override reveals the deployment default again if present; it does not necessarily remove the name from the list.

Select registered names in the agent TOML `skills` array, then publish the configuration. New sessions resolve and pin full instructions and their digest. Registry edits, deletes, and agent replacements never rewrite an existing session's prefix. Start a new session for changed instructions; resuming the old one preserves its old pin. Unknown names fail validation/session creation rather than silently disappearing.

Skills cannot grant tools, workspace access, or credentials. Only server registries define tools, currently `time` and `bash`; a skill can guide their use but cannot install arbitrary capabilities.

Full contract: [account skills registry](https://github.com/kleedaisuki/moesegfault-agents/blob/main/docs/skills.md).
