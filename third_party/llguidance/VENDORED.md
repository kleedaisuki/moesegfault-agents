# Bounded grammar validation dependency

Published crate: `llguidance` 1.9.1 (MIT),
https://static.crates.io/crates/llguidance/llguidance-1.9.1.crate.
The published package records upstream revision
`f0971424ec072d3e4d4196bcc7f31a2f60527df9` with a dirty working-tree marker;
the crate archive SHA-256 and all copied files are recorded in
`source-manifest.json`. Published archive bytes, not a claim of a clean Git tree,
are the source authority.

Implementation files are unchanged. The only manifest patch redirects both
normal and development `serde_json` dependencies to the existing isolated
`deepseek-serde-json` package. LLGuidance needs insertion-ordered JSON internally;
enabling that feature on the platform's serde_json would change established
serialization behavior. Lark/regex enter through strings; no incompatible JSON
Value crosses that package boundary. Toktrie uses ordinary serde_json without
the insertion-order feature.

The platform selects `default-features = false`, `lark` and `wasm`; it does not
enable parallel execution, remote JSON references, JSON Schema validation,
debug grammar logging, or C header generation. The `instant` WebAssembly clock
backend is enabled separately. A 262-token byte environment validates complete
UTF-8 input; it is not a model tokenizer or a claim about token billing.

The caller imposes grammar/input/fuel/state/Earley-item limits and quiet errors.
Budget exhaustion is a validation failure, never acceptance. Actual Worker
binary size and isolate-memory acceptance remain GHA/deployment gates.
