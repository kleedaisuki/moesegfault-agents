//! Native alarm lifetime for one owned ingest generation. D1 owns every job,
//! lease, paid-effect fence, and result; the actor stores only its immutable key.
use crate::util;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use worker::*;
const KEY: &str = "file-ingest.key.v1";

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Key {
    owner: String,
    store: String,
    file: String,
    generation: String,
}
impl Key {
    fn validate(&self) -> Result<()> {
        for value in [&self.owner, &self.store, &self.file, &self.generation] {
            if value.is_empty() || value.len() > 1024 || value.contains('\0') {
                return Err(Error::RustError("invalid_file_ingest_identity".into()));
            }
        }
        Ok(())
    }
    fn name(&self) -> Result<String> {
        Ok(format!(
            "file-ingest:{:x}",
            Sha256::digest(serde_json::to_vec(self)?)
        ))
    }
}

pub(crate) async fn enqueue(
    env: &Env,
    owner: &str,
    store: &str,
    file: &str,
    generation: &str,
) -> Result<()> {
    let key = Key {
        owner: owner.into(),
        store: store.into(),
        file: file.into(),
        generation: generation.into(),
    };
    key.validate()?;
    let namespace = env.durable_object("FILE_INGEST")?;
    let stub = namespace.id_from_name(&key.name()?)?.get_stub()?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_body(Some(serde_json::to_string(&key)?.into()));
    let response = stub
        .fetch_with_request(Request::new_with_init(
            "https://file-ingest.internal/enqueue",
            &init,
        )?)
        .await?;
    if response.status_code() != 202 {
        return Err(Error::RustError("file_ingest_enqueue_failed".into()));
    }
    Ok(())
}

#[durable_object]
pub struct FileIngest {
    state: State,
    env: Env,
    alarm_lock: futures::lock::Mutex<()>,
}
impl DurableObject for FileIngest {
    fn new(state: State, env: Env) -> Self {
        Self {
            state,
            env,
            alarm_lock: futures::lock::Mutex::new(()),
        }
    }
    async fn fetch(&self, mut request: Request) -> Result<Response> {
        if request.method() != Method::Post || request.path() != "/enqueue" {
            return Response::error("Not found", 404);
        }
        let key: Key = request.json().await?;
        key.validate()?;
        if let Some(existing) = self.state.storage().get::<Key>(KEY).await? {
            if existing != key {
                return Response::error("Identity conflict", 409);
            }
        } else {
            self.state.storage().put(KEY, &key).await?;
        }
        if self.state.storage().get_alarm().await?.is_none() {
            self.state
                .storage()
                .set_alarm(util::alarm_at(util::now_ms() + 1))
                .await?;
        }
        Response::empty().map(|response| response.with_status(202))
    }
    async fn alarm(&self) -> Result<Response> {
        let _guard = self.alarm_lock.lock().await;
        let Some(key) = self.state.storage().get::<Key>(KEY).await? else {
            return Response::ok("retired");
        };
        // The next alarm is a crash-recovery wakeup, not permission to replay a
        // submitted provider effect. Existing D1 phases decide every resume.
        self.state
            .storage()
            .set_alarm(util::alarm_at(util::now_ms() + 600_000))
            .await?;
        let result = crate::file_index_scheduler::run_alarm(
            &self.env,
            &key.owner,
            &key.store,
            &key.file,
            &key.generation,
        )
        .await;
        let next = self.next(&key).await?;
        if let Some(next) = next {
            self.state.storage().set_alarm(util::alarm_at(next)).await?;
        } else {
            self.state.storage().delete(KEY).await?;
            self.state.storage().delete_alarm().await?;
        }
        if let Err(error) = result {
            util::emit(
                "file_search.alarm_deferred",
                serde_json::json!({"operation_id":key.name()?,"error_code":match error {Error::RustError(code)=>code,_=>"file_ingest_runtime_error".into()}}),
            );
        }
        Response::ok("file ingest")
    }
}
impl FileIngest {
    async fn next(&self, key: &Key) -> Result<Option<u64>> {
        #[derive(Deserialize)]
        struct Pending {
            retry_at: u64,
            lease_until: u64,
        }
        let row=self.env.d1("DB")?.prepare("SELECT retry_at,lease_until FROM vector_store_files WHERE account_id=?1 AND vector_store_id=?2 AND file_id=?3 AND generation=?4 AND status='in_progress' AND funding_blocked=0 AND removed_at IS NULL")
            .bind(&[key.owner.as_str().into(),key.store.as_str().into(),key.file.as_str().into(),key.generation.as_str().into()])?.first::<Pending>(None).await?;
        Ok(row.map(|row| row.retry_at.max(row.lease_until).max(util::now_ms() + 2000)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actor_identity_is_owner_store_file_and_generation_not_just_a_filename() {
        let key = Key {
            owner: "owner".into(),
            store: "store".into(),
            file: "file".into(),
            generation: "generation".into(),
        };
        key.validate().unwrap();
        let name = key.name().unwrap();
        for alternate in [
            Key {
                owner: "other".into(),
                ..key.clone()
            },
            Key {
                store: "other".into(),
                ..key.clone()
            },
            Key {
                file: "other".into(),
                ..key.clone()
            },
            Key {
                generation: "other".into(),
                ..key.clone()
            },
        ] {
            assert_ne!(name, alternate.name().unwrap());
        }
    }
}
