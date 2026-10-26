//! Effective native context is a view; immutable history remains addressable.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use worker::{Error, Result};

use crate::provider::{
    FunctionCall, Message, ProviderConfig, ToolCall, controls::HarnessControlEffects,
    responses::PreparedRequest,
};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Window {
    pub begin: usize,
    // Retained positions preceding the suffix. Auto fitting selects historical
    // role instructions; rejection cleanup may retain ordinary earlier items.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub protected: Vec<usize>,
}

impl Window {
    fn validate(&self, end: usize) -> Result<()> {
        if self.begin > end
            || self.protected.iter().any(|index| *index >= self.begin)
            || self.protected.windows(2).any(|pair| pair[0] >= pair[1])
        {
            return Err(fault("native_context_window_invalid"));
        }
        Ok(())
    }

    pub(crate) fn contains(&self, index: usize, end: usize) -> bool {
        self.validate(end).is_ok()
            && index < end
            && (index >= self.begin || self.protected.binary_search(&index).is_ok())
    }

    pub(crate) fn indices(&self, end: usize) -> Result<Vec<usize>> {
        self.validate(end)?;
        Ok(self
            .protected
            .iter()
            .copied()
            .chain(self.begin..end)
            .collect())
    }

    pub(crate) fn without(&self, end: usize, excluded: &[usize]) -> Result<Self> {
        self.validate(end)?;
        if excluded.iter().any(|index| *index >= end) {
            return Err(fault("native_context_window_invalid"));
        }
        let excluded = excluded
            .iter()
            .copied()
            .filter(|index| self.contains(*index, end))
            .collect::<BTreeSet<_>>();
        let Some(last) = excluded.last() else {
            return Ok(self.clone());
        };
        let begin = self.begin.max(last + 1);
        let protected = self
            .protected
            .iter()
            .copied()
            .chain(self.begin..begin)
            .filter(|index| !excluded.contains(index))
            .collect();
        Ok(Self { begin, protected })
    }
}

pub(crate) struct Fitted {
    pub prepared: PreparedRequest,
    pub cut: usize,
    pub input_tokens: Option<u64>,
}

fn fault(code: &'static str) -> Error {
    Error::RustError(code.into())
}

pub(crate) fn protected(item: &Value) -> bool {
    matches!(item["role"].as_str(), Some("system" | "developer"))
}

pub(crate) fn kept_indices(input: &[Value], cut: usize) -> Result<Vec<usize>> {
    if input.is_empty() || cut == 0 || cut > input.len() {
        return Err(fault("native_context_window_invalid"));
    }
    Ok((0..cut)
        .filter(|index| *index == 0 || protected(&input[*index]))
        .chain(cut..input.len())
        .collect())
}

fn call(item: &Value) -> Option<&str> {
    match item["type"].as_str()? {
        "function_call" | "custom_tool_call" | "computer_call" | "shell_call"
        | "local_shell_call" | "apply_patch_call" => {
            item["call_id"].as_str().or_else(|| item["id"].as_str())
        }
        "tool_search_call" if item["execution"] == "client" => item["call_id"].as_str(),
        "mcp_approval_request" => item["id"].as_str(),
        _ => None,
    }
}

fn result(item: &Value) -> Option<&str> {
    match item["type"].as_str()? {
        "function_call_output"
        | "custom_tool_call_output"
        | "computer_call_output"
        | "shell_call_output"
        | "apply_patch_call_output"
        | "tool_search_output" => item["call_id"].as_str(),
        "local_shell_call_output" => item["id"].as_str(),
        "mcp_approval_response" => item["approval_request_id"].as_str(),
        _ => None,
    }
}

/// Each candidate starts a user group and never separates a call from its result.
pub(crate) fn cuts(input: &[Value]) -> Vec<usize> {
    let mut pending = BTreeSet::new();
    let mut cuts = vec![];
    let mut droppable = false;
    for (index, item) in input.iter().enumerate().skip(1) {
        if droppable && item["role"] == "user" && pending.is_empty() {
            cuts.push(index);
        }
        if let Some(id) = call(item) {
            pending.insert(id);
        }
        if let Some(id) = result(item) {
            pending.remove(id);
        }
        droppable |= !protected(item);
    }
    cuts
}

pub(crate) fn selected(input: &[Value], cut: usize) -> Result<Vec<Value>> {
    Ok(kept_indices(input, cut)?
        .into_iter()
        .map(|index| input[index].clone())
        .collect())
}

/// Explicit auto truncation uses one shared exact counter. The final prepared
/// request disables upstream truncation: the persisted window is the one sent.
pub(crate) async fn fit(
    config: &ProviderConfig,
    input: &[Value],
    tools: &[Value],
    controls: &Value,
    context_window: u32,
    output_budget: u32,
    mut effects: HarnessControlEffects,
) -> Result<Fitted> {
    if input.is_empty() || !protected(&input[0]) {
        return Err(fault("native_context_window_invalid"));
    }
    let auto = controls["truncation"] == "auto";
    let mut controls = controls.clone();
    if auto {
        controls["truncation"] = json!("disabled");
        effects.truncation = true;
    }
    let build = |input: &[Value]| {
        PreparedRequest::new_with_effects(config, input, tools, &controls, effects)
            .map_err(Error::RustError)
    };
    let prepared = build(input)?;
    if !auto {
        return Ok(Fitted {
            prepared,
            cut: 1,
            input_tokens: None,
        });
    }
    let limit = u64::from(context_window.saturating_sub(output_budget));
    let tokens = crate::responses_auxiliary::count_rendered(config, &prepared)
        .await?
        .input_tokens;
    if tokens <= limit {
        return Ok(Fitted {
            prepared,
            cut: 1,
            input_tokens: Some(tokens),
        });
    }
    let cuts = cuts(input);
    let Some(&latest) = cuts.last() else {
        return Err(fault("context_length_exceeded"));
    };
    let mut best = build(&selected(input, latest)?)?;
    let mut measured = crate::responses_auxiliary::count_rendered(config, &best)
        .await?
        .input_tokens;
    if measured > limit {
        return Err(fault("context_length_exceeded"));
    }
    let mut lower = 0;
    let mut upper = cuts.len() - 1;
    // A verified fitting candidate is retained throughout; tokenization need not
    // be strictly monotone at text boundaries for the final bound to be valid.
    while lower < upper {
        let middle = lower + (upper - lower) / 2;
        let candidate = build(&selected(input, cuts[middle])?)?;
        let tokens = crate::responses_auxiliary::count_rendered(config, &candidate)
            .await?
            .input_tokens;
        if tokens <= limit {
            upper = middle;
            best = candidate;
            measured = tokens;
        } else {
            lower = middle + 1;
        }
    }
    Ok(Fitted {
        prepared: best,
        cut: cuts[upper],
        input_tokens: Some(measured),
    })
}

/// The active legacy view is only a projection. Private native items and source
/// messages remain unchanged; unknown rich items are never copied into metadata.
pub(crate) fn project(items: &[Value]) -> Result<(Vec<Message>, Vec<Message>)> {
    let mut messages: Vec<Message> = vec![];
    let mut instructions = vec![];
    for item in items {
        match item["type"].as_str().unwrap_or("message") {
            "message" => {
                let role = item["role"]
                    .as_str()
                    .ok_or_else(|| fault("invalid_native_context_item"))?;
                let mut message = Message::text(role, "");
                message.content = content(&item["content"]);
                if protected(item) {
                    instructions.push(message);
                } else {
                    messages.push(message);
                }
            }
            "function_call" => {
                let call = ToolCall {
                    id: item["call_id"]
                        .as_str()
                        .ok_or_else(|| fault("invalid_native_context_item"))?
                        .into(),
                    kind: "function".into(),
                    function: FunctionCall {
                        name: item["name"]
                            .as_str()
                            .ok_or_else(|| fault("invalid_native_context_item"))?
                            .into(),
                        arguments: item["arguments"]
                            .as_str()
                            .ok_or_else(|| fault("invalid_native_context_item"))?
                            .into(),
                    },
                };
                if let Some(message) = messages
                    .last_mut()
                    .filter(|message| message.role == "assistant")
                {
                    message.tool_calls.push(call);
                } else {
                    let mut message = Message::text("assistant", "");
                    message.tool_calls.push(call);
                    messages.push(message);
                }
            }
            "function_call_output" => {
                let id = item["call_id"]
                    .as_str()
                    .ok_or_else(|| fault("invalid_native_context_item"))?;
                let mut message = Message::tool_result(id, "");
                message.content = content(&item["output"]);
                messages.push(message);
            }
            "reasoning" => {
                let summary = item["summary"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|part| part["type"] == "summary_text")
                    .filter_map(|part| part["text"].as_str())
                    .collect::<String>();
                if !summary.is_empty() {
                    messages.push(Message::text("assistant", summary));
                }
            }
            _ => {}
        }
    }
    Ok((messages, instructions))
}

fn content(value: &Value) -> Value {
    let Some(parts) = value.as_array() else {
        return value.clone();
    };
    let has_image = parts.iter().any(|part| part["type"] == "input_image");
    if !has_image {
        return json!(
            parts
                .iter()
                .filter_map(|part| part["text"].as_str().or_else(|| part["refusal"].as_str()))
                .collect::<String>()
        );
    }
    Value::Array(parts.iter().filter_map(|part| {
        if part["type"] == "input_image" {
            return Some(json!({"type":"image_url","image_url":{"url":part["image_url"],"detail":part["detail"].as_str().unwrap_or("auto")}}));
        }
        part["text"].as_str().map(|text| json!({"type":"text","text":text}))
    }).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ProviderConfig {
        ProviderConfig {
            protocol: agents_core::ProviderProtocol::Responses,
            base_url: "https://api.deepseek.com/v1".into(),
            api_key: "offline-unused".into(),
            model: "deepseek-flash".into(),
            max_output_tokens: Some(64),
            session_id: None,
            traceparent: None,
        }
    }

    #[test]
    fn native_window_fits_using_the_production_official_counter_without_inference() {
        let config = config();
        let input = vec![
            json!({"type":"message","role":"system","content":"Fixed pin."}),
            json!({"type":"message","role":"user","content":"Old evidence ".repeat(1024)}),
            json!({"type":"message","role":"developer","content":"Persistent policy."}),
            json!({"type":"message","role":"user","content":"Latest goal."}),
        ];
        let source = input.clone();
        let fitted = futures::executor::block_on(fit(
            &config,
            &input,
            &[],
            &json!({"truncation":"auto","reasoning":{"effort":"none"}}),
            128,
            64,
            HarnessControlEffects {
                max_tool_calls: true,
                ..Default::default()
            },
        ))
        .unwrap();
        assert_eq!(fitted.cut, 3);
        assert!(fitted.input_tokens.unwrap() <= 64);
        assert_eq!(fitted.prepared.payload(false)["truncation"], "disabled");
        let actual = crate::deepseek_tokenizer::count_deepseek(&config, &fitted.prepared).unwrap();
        assert_eq!(Some(actual.input_tokens), fitted.input_tokens);
        assert_eq!(input, source);
        assert_eq!(
            fitted.prepared.payload(false)["input"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn native_window_rejects_an_unfit_latest_group_and_keeps_default_count_optional() {
        let config = config();
        let input = vec![
            json!({"role":"system","content":"pin"}),
            json!({"role":"user","content":"old"}),
            json!({"role":"user","content":"Unfit latest ".repeat(1024)}),
        ];
        let error = futures::executor::block_on(fit(
            &config,
            &input,
            &[],
            &json!({"truncation":"auto","reasoning":{"effort":"none"}}),
            128,
            64,
            HarnessControlEffects {
                max_tool_calls: true,
                ..Default::default()
            },
        ))
        .err()
        .unwrap();
        assert!(error.to_string().contains("context_length_exceeded"));
        let default = futures::executor::block_on(fit(
            &config,
            &input,
            &[],
            &json!({}),
            128,
            64,
            HarnessControlEffects {
                max_tool_calls: true,
                ..Default::default()
            },
        ))
        .unwrap();
        assert_eq!(default.cut, 1);
        assert_eq!(default.input_tokens, None);
    }

    #[test]
    fn native_window_preserves_instructions_and_whole_tool_groups() {
        let input = vec![
            json!({"role":"system","content":"pin"}),
            json!({"role":"user","content":"old"}),
            json!({"type":"function_call","call_id":"call","name":"time","arguments":"{}"}),
            json!({"role":"user","content":"accepted before result"}),
            json!({"role":"developer","content":"keep"}),
            json!({"type":"function_call_output","call_id":"call","output":"result"}),
            json!({"role":"user","content":"latest"}),
        ];
        assert_eq!(cuts(&input), vec![6]);
        assert_eq!(kept_indices(&input, 6).unwrap(), vec![0, 4, 6]);
    }

    #[test]
    fn native_window_server_discovery_null_call_id_is_not_an_open_client_call() {
        let input = vec![
            json!({"role":"system","content":"pin"}),
            json!({"role":"user","content":"old"}),
            json!({"type":"tool_search_call","execution":"server","id":"item","call_id":null}),
            json!({"type":"tool_search_output","execution":"server","call_id":null,"tools":[]}),
            json!({"role":"user","content":"latest"}),
        ];
        assert_eq!(cuts(&input), vec![4]);
        let mut input = input;
        input[2]["execution"] = json!("client");
        input[2]["call_id"] = json!("open_call");
        assert!(cuts(&input).is_empty());
    }

    #[test]
    fn native_window_reference_rejects_aliases_and_keeps_absolute_indexes() {
        let view = Window {
            begin: 5,
            protected: vec![0, 3],
        };
        assert_eq!(view.indices(8).unwrap(), vec![0, 3, 5, 6, 7]);
        assert!(
            Window {
                begin: 2,
                protected: vec![2]
            }
            .indices(3)
            .is_err()
        );
        assert!(
            Window {
                begin: 3,
                protected: vec![1, 1]
            }
            .indices(4)
            .is_err()
        );
        assert!(
            Window {
                begin: 3,
                protected: vec![]
            }
            .indices(2)
            .is_err()
        );
    }

    #[test]
    fn native_window_rejection_excludes_holes_without_resurrecting_prior_drops() {
        let before = Window {
            begin: 5,
            protected: vec![0, 3],
        };
        let after = before.without(10, &[7]).unwrap();
        assert_eq!(after.indices(10).unwrap(), vec![0, 3, 5, 6, 8, 9]);
        assert_eq!(after.without(10, &[7]).unwrap(), after);
        assert_eq!(before.indices(10).unwrap(), vec![0, 3, 5, 6, 7, 8, 9]);
        assert_eq!(
            before.without(10, &[3]).unwrap().indices(10).unwrap(),
            vec![0, 5, 6, 7, 8, 9]
        );
        assert!(before.without(10, &[10]).is_err());
    }

    #[test]
    fn native_window_absence_preserves_stored_reference_bytes() {
        let old = json!({"generation":"old","source_session_id":"session","end":3,"provider_binding":"binding"});
        let reference: crate::session::branch::NativeHistoryRef =
            serde_json::from_value(old.clone()).unwrap();
        assert!(reference.window.is_none());
        assert_eq!(
            serde_json::to_string(&reference).unwrap(),
            r#"{"generation":"old","source_session_id":"session","end":3,"provider_binding":"binding"}"#
        );
        assert_eq!(serde_json::to_value(reference).unwrap(), old);
    }

    #[test]
    fn native_window_projection_keeps_private_reasoning_out_of_the_legacy_view() {
        let items = vec![
            json!({"type":"message","role":"developer","content":"policy"}),
            json!({"type":"message","role":"user","content":"latest"}),
            json!({"type":"reasoning","content":"PRIVATE_REASONING","encrypted_content":"PRIVATE_CIPHERTEXT"}),
            json!({"type":"function_call","call_id":"call","name":"time","arguments":"{}"}),
            json!({"type":"function_call_output","call_id":"call","output":"known result"}),
        ];
        let original = items.clone();
        let (messages, instructions) = project(&items).unwrap();
        assert_eq!(items, original);
        assert_eq!(instructions[0].role, "developer");
        assert_eq!(messages[1].tool_calls[0].id, "call");
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("call"));
        assert!(
            !serde_json::to_string(&messages)
                .unwrap()
                .contains("PRIVATE_")
        );
    }

    #[test]
    fn native_window_visible_reasoning_summary_is_portable_without_ciphertext() {
        let (messages, _) = project(&[json!({"type":"reasoning","content":"PRIVATE_CHAIN",
            "encrypted_content":"PRIVATE_CIPHER", "summary":[{"type":"summary_text","text":"Visible plan."}]})]).unwrap();
        assert_eq!(messages[0].content, "Visible plan.");
        assert!(
            !serde_json::to_string(&messages)
                .unwrap()
                .contains("PRIVATE_")
        );
    }

    proptest::proptest! {
        #[test]
        fn native_window_keeps_latest_user_and_never_mutates_source(count in 2usize..128, cut in 1usize..128) {
            let mut input = vec![json!({"role":"system","content":"fixed"})];
            input.extend((0..count).map(|index|json!({"role":"user","content":index.to_string()})));
            let source = input.clone();
            let cut = cut.min(count);
            let retained = selected(&input, cut).unwrap();
            proptest::prop_assert_eq!(&input, &source);
            proptest::prop_assert_eq!(&retained[0], &source[0]);
            proptest::prop_assert_eq!(retained.last(), source.last());
        }
    }
}
