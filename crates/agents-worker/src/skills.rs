//! Account-owned instruction registry. Sessions copy resolved content, not mutable heads.

use agents_core::RegisteredSkill;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use worker::{Env, Method, Request, Response, Result};

use crate::{auth::AuthContext, util};

const MAX_BODY_BYTES: usize = 64 * 1024;
const MAX_INSTRUCTIONS_BYTES: usize = 48 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateSkill {
    name: String,
    instructions: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateSkill {
    instructions: String,
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-')
}

fn valid_instructions(instructions: &str) -> bool {
    !instructions.trim().is_empty() && instructions.len() <= MAX_INSTRUCTIONS_BYTES
}

fn version(name: &str, instructions: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"magents.skill.v1\0");
    hash.update(name.as_bytes());
    hash.update(b"\0");
    hash.update(instructions.as_bytes());
    format!("{:x}", hash.finalize())
}

fn deployment(env: &Env) -> Result<Vec<RegisteredSkill>> {
    let Some(raw) = env.var("SKILLS_REGISTRY_JSON").ok() else {
        return Ok(Vec::new());
    };
    let skills: Vec<RegisteredSkill> = serde_json::from_str(&raw.to_string())
        .map_err(|_| worker::Error::RustError("invalid_deployment_skills".into()))?;
    let mut seen = std::collections::BTreeSet::new();
    if skills.len() > 50
        || skills.iter().any(|s| {
            !valid_name(&s.name)
                || !valid_instructions(&s.instructions)
                || s.version.is_empty()
                || s.version.len() > 128
                || !seen.insert(s.name.clone())
        })
    {
        return Err(worker::Error::RustError("invalid_deployment_skills".into()));
    }
    Ok(skills)
}

async fn owned(env: &Env, owner: &str) -> Result<Vec<RegisteredSkill>> {
    env.d1("DB")?.prepare("SELECT s.name,s.version,v.instructions FROM skills s JOIN skill_versions v USING(account_id,name,version) WHERE s.account_id=?1 ORDER BY s.name")
        .bind(&[owner.into()])?.all().await?.results::<RegisteredSkill>()
}

/// Resolve once on session creation; callers persist all returned bytes in PinnedContext.
pub async fn resolve(env: &Env, owner: &str, names: &[String]) -> Result<Vec<RegisteredSkill>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    if names.len() > 50 || names.iter().any(|name| !valid_name(name)) {
        return Err(worker::Error::RustError("invalid_skill".into()));
    }
    let mut registry = std::collections::BTreeMap::new();
    for skill in deployment(env)?.into_iter().chain(owned(env, owner).await?) {
        registry.insert(skill.name.clone(), skill);
    }
    let mut selected = std::collections::BTreeMap::new();
    for name in names {
        let skill = registry
            .get(name)
            .ok_or_else(|| worker::Error::RustError("unknown_skill".into()))?;
        selected.insert(name.clone(), skill.clone());
    }
    Ok(selected.into_values().collect())
}

pub async fn handle(
    mut req: Request,
    env: &Env,
    context: &AuthContext,
    name: Option<&str>,
) -> Result<Response> {
    if context.via_api_key {
        return util::json_error(
            403,
            "human_authentication_required",
            "Sign in to manage skills",
        );
    }
    if name.is_some_and(|name| !valid_name(name)) {
        return util::json_error(
            400,
            "invalid_skill",
            "Use 1-64 lowercase letters, digits, underscores or hyphens",
        );
    }
    let owner = context.account_id.as_str();
    match (req.method(), name) {
        (Method::Get, name) => read(env, owner, name).await,
        (Method::Post, None) => {
            let value = match body(&mut req).await {
                Ok(v) => v,
                Err(response) => return Ok(response),
            };
            let Ok(input) = serde_json::from_value::<CreateSkill>(value) else {
                return invalid_body();
            };
            write(env, owner, &input.name, &input.instructions, true).await
        }
        (Method::Put, Some(name)) => {
            let value = match body(&mut req).await {
                Ok(v) => v,
                Err(response) => return Ok(response),
            };
            let Ok(input) = serde_json::from_value::<UpdateSkill>(value) else {
                return invalid_body();
            };
            write(env, owner, name, &input.instructions, false).await
        }
        (Method::Delete, Some(name)) => remove(env, owner, name).await,
        _ => util::json_error(405, "method_not_allowed", "Method not allowed"),
    }
}

async fn read(env: &Env, owner: &str, name: Option<&str>) -> Result<Response> {
    let mut entries = std::collections::BTreeMap::new();
    for (source, skills) in [
        ("deployment", deployment(env)?),
        ("account", owned(env, owner).await?),
    ] {
        for skill in skills {
            entries.insert(skill.name.clone(), json!({"name":skill.name,"version":skill.version,"instructions":skill.instructions,"source":source}));
        }
    }
    match name {
        Some(name) => match entries.get(name) {
            Some(skill) => Response::from_json(skill),
            None => util::json_error(404, "skill_not_found", "Skill not found"),
        },
        None => Response::from_json(
            &json!({"object":"list","data":entries.into_values().collect::<Vec<_>>()}),
        ),
    }
}

async fn write(
    env: &Env,
    owner: &str,
    name: &str,
    instructions: &str,
    create: bool,
) -> Result<Response> {
    if !valid_name(name) || !valid_instructions(instructions) {
        return invalid_body();
    }
    let db = env.d1("DB")?;
    let exists = db
        .prepare("SELECT name FROM skills WHERE account_id=?1 AND name=?2")
        .bind(&[owner.into(), name.into()])?
        .first::<Value>(None)
        .await?
        .is_some();
    if create && exists {
        return util::json_error(
            409,
            "skill_exists",
            "Skill already exists; use PUT to update",
        );
    }
    if !create && !exists {
        return util::json_error(404, "skill_not_found", "Account skill not found");
    }
    let revision = version(name, instructions);
    let now = util::now_ms() as f64;
    let insert = db.prepare("INSERT OR IGNORE INTO skill_versions(account_id,name,version,instructions,created_at_ms) SELECT ?1,?2,?3,?4,?5 WHERE ?6=1 OR EXISTS(SELECT 1 FROM skills WHERE account_id=?1 AND name=?2)")
        .bind(&[owner.into(), name.into(), revision.clone().into(), instructions.into(), now.into(), (if create { 1.0 } else { 0.0 }).into()])?;
    let sql = if create {
        "INSERT INTO skills(account_id,name,version,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,?4) RETURNING name"
    } else {
        "UPDATE skills SET version=?3,updated_at_ms=?4 WHERE account_id=?1 AND name=?2 RETURNING name"
    };
    let head = db.prepare(sql).bind(&[
        owner.into(),
        name.into(),
        revision.clone().into(),
        now.into(),
    ])?;
    // D1 batch is transactional: a quota/race failure cannot leave an orphan version.
    let results = match db.batch(vec![insert, head]).await {
        Ok(results) => results,
        Err(error) => {
            let message = error.to_string();
            if message.contains("skill_limit") {
                return util::json_error(
                    409,
                    "skill_limit",
                    "At most 50 account skills are allowed",
                );
            }
            if create
                && message.contains("UNIQUE constraint failed: skills.account_id, skills.name")
            {
                return util::json_error(
                    409,
                    "skill_exists",
                    "Skill already exists; use PUT to update",
                );
            }
            return Err(error);
        }
    };
    if results[1].results::<Value>()?.is_empty() {
        return util::json_error(404, "skill_not_found", "Account skill not found");
    }
    Response::from_json(
        &json!({"name":name,"version":revision,"instructions":instructions,"source":"account"}),
    )
    .map(|response| response.with_status(if create { 201 } else { 200 }))
}

async fn remove(env: &Env, owner: &str, name: &str) -> Result<Response> {
    let db = env.d1("DB")?;
    let head = db
        .prepare("DELETE FROM skills WHERE account_id=?1 AND name=?2 RETURNING name")
        .bind(&[owner.into(), name.into()])?;
    let versions = db
        .prepare("DELETE FROM skill_versions WHERE account_id=?1 AND name=?2")
        .bind(&[owner.into(), name.into()])?;
    let results = db.batch(vec![head, versions]).await?;
    if results[0].results::<Value>()?.is_empty() {
        return util::json_error(404, "skill_not_found", "Account skill not found");
    }
    Response::empty().map(|response| response.with_status(204))
}

fn invalid_body() -> Result<Response> {
    util::json_error(
        400,
        "invalid_skill",
        "Supply only name (create) and instructions; instructions must contain 1-49152 UTF-8 bytes",
    )
}

async fn body(req: &mut Request) -> std::result::Result<Value, Response> {
    let invalid = || invalid_body().expect("JSON error response");
    let mut stream = req.stream().map_err(|_| invalid())?;
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| invalid())?;
        if chunk.len() > MAX_BODY_BYTES.saturating_sub(bytes.len()) {
            return Err(
                util::json_error(413, "body_too_large", "Body exceeds 64 KiB")
                    .expect("JSON error response"),
            );
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| invalid())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_bounds_and_rejects_schema_injection() {
        assert!(valid_name("rust-review_2"));
        for name in ["", "../escape", "Other", "two names", "é"] {
            assert!(!valid_name(name));
        }
        assert!(!valid_name(&"a".repeat(65)));
        assert!(!valid_instructions(" \n"));
        assert!(valid_instructions(&"a".repeat(MAX_INSTRUCTIONS_BYTES)));
        assert!(!valid_instructions(&"é".repeat(MAX_INSTRUCTIONS_BYTES)));
        assert!(
            serde_json::from_value::<CreateSkill>(
                json!({"name":"a","instructions":"b","tools":[]})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<UpdateSkill>(json!({"instructions":"b","version":"fake"}))
                .is_err()
        );
    }

    #[test]
    fn content_versions_are_deterministic_and_sensitive_to_every_field() {
        let original = version("review", "Review carefully.\n");
        assert_eq!(original.len(), 64);
        assert_eq!(original, version("review", "Review carefully.\n"));
        assert_ne!(original, version("review2", "Review carefully.\n"));
        assert_ne!(original, version("review", "Review carefully."));
    }
}
