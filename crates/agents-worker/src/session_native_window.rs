//! Publish a fitted view without rewriting either immutable history.

use super::*;

impl SessionInner {
    /// The caller funds the warm-up's count probes without model admission.
    pub(super) async fn prepare_warm_native_window(&self) -> Result<()> {
        let (record, limit, definitions, mut options) = {
            let borrowed = self.data.borrow();
            let data = borrowed.as_ref().unwrap();
            let response = data
                .run
                .as_ref()
                .and_then(|run| run.response.as_ref())
                .ok_or_else(|| Error::RustError("response_not_found".into()))?;
            let mut definitions = tool_definitions(&data.record);
            definitions.extend(
                response
                    .request
                    .function_definitions()
                    .map_err(Error::RustError)?,
            );
            definitions.extend(response.bound_tools.definitions()?);
            let mut options = response.request.generation_options();
            if let Some(choice) = options.tool_choice.take() {
                options.tool_choice = Some(response.bound_tools.rewrite_choice(choice)?);
            }
            (
                data.record.clone(),
                response.request.max_output_tokens,
                definitions,
                options,
            )
        };
        let mut config = self.provider_config(&record).await?;
        if let Some(limit) = limit {
            config.max_output_tokens = Some(limit.min(record.pinned.model.max_output_tokens));
        }
        self.prepare_native_generation(&config, &definitions, &mut options)
            .await?;
        Ok(())
    }

    pub(super) async fn apply_native_window(
        &self,
        expected: &branch::NativeHistoryRef,
        input: &[Value],
        cut: usize,
        tokens: u64,
    ) -> Result<()> {
        let _guard = self.persistence.lock().await;
        let mut borrowed = self.data.borrow_mut();
        let data = borrowed.as_mut().unwrap();
        let context = data
            .native_context
            .as_mut()
            .ok_or_else(|| Error::RustError("native_context_unavailable".into()))?;
        if context.history != *expected || context.messages_end != data.messages.len() {
            return Err(Error::RustError("native_context_window_changed".into()));
        }
        if cut == 1 {
            data.input_tokens = tokens;
            data.measured_messages = data.messages.len();
            return Ok(());
        }
        let indices = expected
            .window
            .as_ref()
            .map(|window| window.indices(expected.end))
            .transpose()?
            .unwrap_or_else(|| (0..expected.end).collect());
        if indices.len().checked_add(1) != Some(input.len()) || cut >= input.len() {
            return Err(Error::RustError("native_context_window_invalid".into()));
        }
        let begin = indices[cut - 1];
        let kept = crate::native_window::kept_indices(input, cut)?;
        let protected = kept
            .iter()
            .copied()
            .filter(|index| *index > 0 && *index < cut)
            .map(|index| indices[index - 1])
            .collect();
        let selected = kept
            .into_iter()
            .filter(|index| *index > 0)
            .map(|index| input[index].clone())
            .collect::<Vec<_>>();
        let (messages, instructions) = crate::native_window::project(&selected)?;
        let prefix = data
            .messages
            .first()
            .cloned()
            .ok_or_else(|| Error::RustError("native_context_message_boundary".into()))?;
        data.messages = std::iter::once(prefix).chain(messages).collect();
        data.history_instructions = instructions;
        data.input_tokens = tokens;
        data.measured_messages = data.messages.len();
        context.messages_end = data.messages.len();
        context.history.window = Some(crate::native_window::Window { begin, protected });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_window_count_capability_failures_keep_machine_readable_codes() {
        for code in [
            "token_count_unavailable",
            "token_count_encrypted_reasoning_unsupported",
            "token_count_media_unsupported",
            "token_count_model_profile_unavailable",
        ] {
            assert_eq!(safe_error_code(&Error::RustError(code.into())), code);
        }
    }
}
