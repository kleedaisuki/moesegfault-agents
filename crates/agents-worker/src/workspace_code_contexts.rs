//! Resource inspection and retirement never restore or start a physical VM.
use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextIdentity {
    container_id: String,
}

impl Workspace<'_> {
    pub async fn python_context(&self) -> Result<Option<String>> {
        if !self.container()?.running() {
            return Ok(None);
        }
        let response = self
            .request(Method::Get, "/v1/python/contexts", None, &[])
            .await?;
        if response.status_code() == 404 {
            return Ok(None);
        }
        let context: ContextIdentity = decode(response).await?;
        if !container_id(&context.container_id) {
            return Err(fail("code_protocol_error"));
        }
        Ok(Some(context.container_id))
    }

    pub async fn python_context_live(&self, id: &str, boot: &str) -> Result<bool> {
        if !container_id(id) {
            return Err(fail("invalid_code_context"));
        }
        if !self.container()?.running() {
            return Ok(false);
        }
        let health: Value = decode(self.request(Method::Get, "/health", None, &[]).await?).await?;
        if health["boot_id"].as_str() != Some(boot) {
            return Ok(false);
        }
        let response = self
            .request(Method::Get, &format!("/v1/python/contexts/{id}"), None, &[])
            .await?;
        match response.status_code() {
            200 => {
                #[derive(Deserialize)]
                struct Status {
                    container_id: String,
                    status: String,
                }
                let context: Status = decode(response).await?;
                if context.container_id != id || context.status != "running" {
                    return Err(fail("code_protocol_error"));
                }
                Ok(true)
            }
            404 => Ok(false),
            _ => Err(fail("code_context_inspect_unavailable")),
        }
    }

    pub async fn retire_python_context(&self, id: &str) -> Result<bool> {
        if !container_id(id) {
            return Err(fail("invalid_code_context"));
        }
        if !self.container()?.running() {
            return Ok(true);
        }
        let response = self
            .request(
                Method::Delete,
                &format!("/v1/python/contexts/{id}"),
                None,
                &[],
            )
            .await?;
        match response.status_code() {
            204 | 404 => Ok(true),
            409 => Ok(false),
            _ => Err(fail("code_context_retire_unavailable")),
        }
    }

    pub async fn python_publication_pending(&self) -> Result<bool> {
        Ok(self
            .state
            .storage()
            .get::<BTreeSet<String>>(PENDING)
            .await?
            .is_some_and(|pending| !pending.is_empty()))
    }

    pub async fn physical_workspace_running(&self) -> Result<bool> {
        Ok(self.container()?.running())
    }
}
