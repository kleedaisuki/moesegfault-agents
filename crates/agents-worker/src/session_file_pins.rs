use super::*;

impl SessionInner {
    pub(super) async fn reconcile_file_pins(&self) -> Result<Response> {
        let _guard = self.persistence.lock().await;
        let record = self
            .data
            .borrow()
            .as_ref()
            .map(|data| data.record.clone())
            .ok_or_else(|| Error::RustError("session_not_initialized".into()))?;
        let cursor_key = "file-pins:reconcile-cursor";
        let cursor = self
            .state
            .storage()
            .get::<String>(cursor_key)
            .await?
            .unwrap_or_default();
        let mut responses = crate::space::list_response_pins_after(
            &self.env,
            &record.owner_id,
            &record.id,
            &cursor,
        )
        .await?;
        if responses.is_empty() && !cursor.is_empty() {
            responses =
                crate::space::list_response_pins(&self.env, &record.owner_id, &record.id).await?;
        }
        let mut retained = 0usize;
        let mut released = 0usize;
        let mut deferred = 0usize;
        for id in &responses {
            let active = self
                .data
                .borrow()
                .as_ref()
                .and_then(|data| data.run.as_ref())
                .is_some_and(|run| run.id == *id);
            if active {
                retained += 1;
                continue;
            }
            let receipt = match self.read_response(id).await {
                Ok(receipt) => receipt,
                Err(_) => {
                    deferred += 1;
                    continue;
                }
            };
            if receipt.as_ref().is_some_and(|response| {
                !matches!(
                    response.status.as_str(),
                    "completed" | "failed" | "incomplete" | "cancelled"
                )
            }) {
                deferred += 1;
                continue;
            }
            // Admission holds this same mutex from acquiring pins through the
            // canonical DO commit. No receipt here therefore proves an orphan,
            // rather than merely observing an in-flight D1 acknowledgement.
            if crate::space::release_response_files(&self.env, &record.owner_id, &record.id, id)
                .await
                .is_err()
            {
                deferred += 1;
            } else {
                released += 1;
            }
        }
        if let Some(last) = responses.last() {
            self.state.storage().put(cursor_key, last).await?;
        } else {
            self.state.storage().delete(cursor_key).await?;
        }
        if deferred > 0 {
            util::emit(
                "response.file_pin_reconcile_deferred",
                json!({"session_id":record.id,"deferred":deferred}),
            );
        }
        Response::from_json(
            &json!({"retained":retained,"released":released,"deferred":deferred,"has_more":responses.len() == 128}),
        )
    }
}
