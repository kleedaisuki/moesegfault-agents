//! Hosted-tool authority is resolved before inference and retained per Response.

use std::collections::BTreeSet;

use super::*;
use crate::{
    hosted_bindings::{BoundTools, MemoryTier},
    responses_wire::ResponseRequest,
};

impl SessionInner {
    /// Counting uses the same pure binding renderer. This Session additionally
    /// constrains compute to its immutable, already selected runtime profile.
    pub(super) async fn resolve_hosted_bindings(
        &self,
        request: &ResponseRequest,
        record: &SessionRecord,
    ) -> Result<BoundTools> {
        crate::hosted_tools::require_declared_runtimes(&self.env, &request.tools)?;
        let declarations = request
            .tools
            .iter()
            .map(crate::responses_wire::ResponseTool::to_wire_value)
            .collect::<Vec<_>>();
        if !has_hosted_tools(request) {
            return BoundTools::resolve(
                &declarations,
                &record.pinned.tool_services,
                &Default::default(),
                &[],
            );
        }
        let mut capabilities = crate::hosted_tools::capabilities(&self.env)?;
        capabilities.container_memory = Some(MemoryTier::from_core(record.runtime_profile));
        let mut image_files = BTreeSet::new();
        collect_image_files(&request.input, &mut image_files);
        let bindings = BoundTools::resolve(
            &declarations,
            &record.pinned.tool_services,
            &capabilities,
            &image_files.into_iter().collect::<Vec<_>>(),
        )?;
        bindings.validate_owned(&self.env, &record.owner_id).await?;
        Ok(bindings)
    }
}

fn has_hosted_tools(request: &ResponseRequest) -> bool {
    request.tools.iter().any(|tool| {
        matches!(
            tool.kind.as_str(),
            "code_interpreter"
                | "image_generation"
                | "web_search"
                | "web_search_2025_08_26"
                | "web_search_preview"
                | "web_search_preview_2025_03_11"
                | "file_search"
        )
    })
}

fn collect_image_files(value: &Value, ids: &mut BTreeSet<String>) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_image_files(item, ids);
            }
        }
        Value::Object(fields) => {
            if fields.get("type").and_then(Value::as_str) == Some("input_image")
                && let Some(id) = fields.get("file_id").and_then(Value::as_str)
            {
                ids.insert(id.into());
            }
            for value in fields.values() {
                collect_image_files(value, ids);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn original_image_authority_excludes_unrelated_file_parts_and_deduplicates() {
        let mut files = BTreeSet::new();
        collect_image_files(
            &json!([
                {"role":"user","content":[
                    {"type":"input_image","file_id":"file-image"},
                    {"type":"input_file","file_id":"file-not-image"},
                    {"type":"input_image","file_id":"file-image"},
                    {"type":"input_image","image_url":"data:image/png;base64,AAAA"}
                ]}
            ]),
            &mut files,
        );
        assert_eq!(files, BTreeSet::from(["file-image".into()]));
    }
}
