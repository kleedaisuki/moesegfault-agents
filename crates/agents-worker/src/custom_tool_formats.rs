//! Custom inputs are raw strings, validated before completed calls can publish.
//! Portable function envelopes provide transport, not grammar-guided decoding.

use std::{cell::RefCell, sync::Arc};

use agents_core::ProviderProtocol;
use llguidance::{
    Logger,
    api::{GrammarInit, ParserLimits, TopLevelGrammar},
    earley::{CGrammar, Parser, perf::ParserPerfCounters},
    toktrie::ApproximateTokEnv,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::provider::ProviderConfig;

const MAX_GRAMMAR_BYTES: usize = 64 * 1024;
const MAX_DECLARATIONS: usize = 128;
const MAX_NAMESPACE_DEPTH: usize = 8;
pub(crate) const MAX_INPUT_BYTES: usize = 64 * 1024;
const MAX_TOTAL_ITEMS: usize = 200_000;

struct CachedGrammar {
    digest: String,
    grammar: Arc<CGrammar>,
}

thread_local! {
    static GRAMMAR: RefCell<Option<CachedGrammar>> = const { RefCell::new(None) };
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GrammarSyntax {
    Lark,
    Regex,
}

// Definitions may contain sensitive application structure. Do not derive Debug
// or interpolate a serde error containing caller-controlled input into diagnostics.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum InputFormat {
    Text,
    Grammar {
        syntax: GrammarSyntax,
        definition: String,
    },
}

pub(crate) fn parse_format(format: Option<&Value>) -> Result<InputFormat, String> {
    let Some(format) = format else {
        return Ok(InputFormat::Text);
    };
    let parsed: InputFormat = serde_json::from_value(format.clone())
        .map_err(|_| "invalid_custom_tool_format".to_owned())?;
    if let InputFormat::Grammar { definition, .. } = &parsed
        && (definition.trim().is_empty()
            || definition.len() > MAX_GRAMMAR_BYTES
            || definition.contains('\0'))
    {
        return Err("invalid_custom_tool_grammar".into());
    }
    Ok(parsed)
}

/// Called before inference admission and again for retained prepared contexts.
/// Capability is supplied by the native codec or the function-input bridge;
/// neither a provider name nor a model prefix changes language validation.
pub(crate) fn validate_provider(
    config: &ProviderConfig,
    declarations: &[Value],
) -> Result<(), String> {
    let mut remaining = MAX_DECLARATIONS;
    validate_declarations(config.protocol, declarations, 0, &mut remaining)
}

fn validate_declarations(
    protocol: ProviderProtocol,
    declarations: &[Value],
    depth: usize,
    remaining: &mut usize,
) -> Result<(), String> {
    if depth > MAX_NAMESPACE_DEPTH || declarations.len() > *remaining {
        return Err("custom_tool_declaration_limit".into());
    }
    *remaining -= declarations.len();
    for declaration in declarations {
        match declaration["type"].as_str() {
            Some("namespace") => {
                let children = declaration["tools"]
                    .as_array()
                    .ok_or("invalid_custom_tool_namespace")?;
                validate_declarations(protocol, children, depth + 1, remaining)?;
            }
            Some("custom") => validate_custom(protocol, declaration)?,
            _ => {}
        }
    }
    Ok(())
}

fn validate_custom(protocol: ProviderProtocol, declaration: &Value) -> Result<(), String> {
    if protocol != ProviderProtocol::Responses {
        return Err("custom_tools_require_native_responses".into());
    }
    validate_tool(declaration)
}

pub(crate) fn validate_tool(declaration: &Value) -> Result<(), String> {
    if declaration["type"] != "custom"
        || declaration["name"].as_str().is_none_or(|name| {
            name.is_empty()
                || name.len() > 256
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        })
    {
        return Err("invalid_custom_tool_declaration".into());
    }
    let format = parse_format(declaration.get("format"))?;
    if let InputFormat::Grammar { .. } = &format {
        compiled(&format)?;
    }
    Ok(())
}

/// The selected declaration alone carries its grammar guidance. Deferred tools
/// remain absent until discovery; no union grammar or hidden repair call exists.
pub(crate) fn bridge_tool(tool: &Value, alias: &str) -> Result<Value, String> {
    validate_tool(tool)?;
    if !alias.starts_with("__agents_")
        || alias.len() > 64
        || !alias
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err("invalid_custom_tool_alias".into());
    }
    let mut description = tool["description"].as_str().unwrap_or_default().to_owned();
    description.push_str("\nReturn the raw tool input in the input string. The server validates the complete input before this caller tool is published; invalid input ends the response without an automatic repair request.");
    if let InputFormat::Grammar { syntax, definition } = parse_format(tool.get("format"))? {
        let syntax = match syntax {
            GrammarSyntax::Lark => "Lark",
            GrammarSyntax::Regex => "regex",
        };
        description.push_str(&format!(
            "\nThe input must match this {syntax} grammar:\n{definition}"
        ));
    }
    Ok(
        json!({"type":"function","name":alias,"description":description,"strict":true,
        "parameters":{"type":"object","properties":{"input":{"type":"string","maxLength":MAX_INPUT_BYTES}},
            "required":["input"],"additionalProperties":false}}),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InputEnvelope {
    input: String,
}

pub(crate) fn restore_input(arguments: &str) -> Result<String, String> {
    // JSON escaping can expand each UTF-8 byte to six ASCII bytes.
    if arguments.len() > MAX_INPUT_BYTES * 6 + 64 {
        return Err("custom_tool_input_limit".into());
    }
    let envelope: InputEnvelope =
        serde_json::from_str(arguments).map_err(|_| "invalid_custom_tool_envelope".to_owned())?;
    if envelope.input.len() > MAX_INPUT_BYTES {
        return Err("custom_tool_input_limit".into());
    }
    Ok(envelope.input)
}

pub(crate) fn requires_validation(tool: &Value) -> bool {
    tool["type"] == "custom"
}

/// This is a server acceptance gate, not constrained model decoding. Call it
/// only on completed inputs, after retaining known provider usage and before
/// exposing a successful client call or permitting an associated side effect.
pub(crate) fn validate_completed_input(tool: &Value, input: &str) -> Result<(), String> {
    if input.len() > MAX_INPUT_BYTES {
        return Err("custom_tool_input_limit".into());
    }
    let format = parse_format(tool.get("format"))?;
    if matches!(format, InputFormat::Text) {
        return Ok(());
    }
    let grammar = compiled(&format)?;
    let mut parser = Parser::new(
        ApproximateTokEnv::single_byte_env(),
        grammar,
        limits(),
        Arc::new(ParserPerfCounters::default()),
    )
    .map_err(|_| "custom_tool_validation_unavailable".to_owned())?;
    for byte in input.as_bytes() {
        let backtrack = parser
            .apply_token(&[*byte], u32::from(*byte))
            .map_err(|_| "custom_tool_input_invalid".to_owned())?;
        if backtrack != 0 || parser.stats().all_items > MAX_TOTAL_ITEMS {
            return Err("custom_tool_validation_limit".into());
        }
    }
    let accepting = parser.is_accepting();
    if parser.stats().all_items > MAX_TOTAL_ITEMS || parser.get_error().is_some() {
        return Err("custom_tool_validation_limit".into());
    }
    if !accepting
        || parser.get_bytes() != input.as_bytes()
        || !parser.currently_forced_bytes().is_empty()
    {
        return Err("custom_tool_input_invalid".into());
    }
    Ok(())
}

fn limits() -> ParserLimits {
    ParserLimits {
        max_items_in_row: 256,
        initial_lexer_fuel: 100_000,
        step_lexer_fuel: 20_000,
        step_max_items: 4_096,
        max_lexer_states: 4_096,
        max_grammar_size: 8_192,
        precompute_large_lexemes: false,
        verbose_errors: false,
    }
}

fn compiled(format: &InputFormat) -> Result<Arc<CGrammar>, String> {
    let InputFormat::Grammar { syntax, definition } = format else {
        return Err("custom_tool_grammar_required".into());
    };
    let syntax_id = match syntax {
        GrammarSyntax::Lark => "lark",
        GrammarSyntax::Regex => "regex",
    };
    let digest = format!(
        "{:x}",
        Sha256::digest(
            [
                b"llguidance-1.9.1/server-input-v1:".as_slice(),
                syntax_id.as_bytes(),
                b":",
                definition.as_bytes()
            ]
            .concat()
        )
    );
    GRAMMAR.with(|cache| {
        if let Some(entry) = cache
            .borrow()
            .as_ref()
            .filter(|entry| entry.digest == digest)
        {
            return Ok(Arc::clone(&entry.grammar));
        }
        // Do not keep two unrelated compiled grammars warm in a 128 MiB isolate.
        cache.borrow_mut().take();
        let grammar = match syntax {
            GrammarSyntax::Lark => TopLevelGrammar::from_lark(definition.clone()),
            GrammarSyntax::Regex => TopLevelGrammar::from_regex(definition),
        };
        let grammar = GrammarInit::Serialized(grammar)
            .to_cgrammar(None, &mut Logger::new(0, 0), limits(), Vec::new())
            .map_err(|_| "custom_tool_grammar_unavailable".to_owned())?;
        let lexer = grammar.lexer_spec();
        if lexer.special_token_rx.is_some()
            || lexer.has_max_tokens
            || lexer.has_stop
            || lexer.has_temperature
        {
            // Model-token and generation-control extensions cannot be proven
            // with a byte alphabet. Do not silently reinterpret their meaning.
            return Err("custom_tool_generation_directive_unavailable".into());
        }
        *cache.borrow_mut() = Some(CachedGrammar {
            digest,
            grammar: Arc::clone(&grammar),
        });
        Ok(grammar)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config(host: &str) -> ProviderConfig {
        ProviderConfig {
            protocol: ProviderProtocol::Responses,
            base_url: format!("https://{host}/v1"),
            api_key: "not-part-of-the-format".into(),
            model: "selected-native-model".into(),
            max_output_tokens: Some(512),
            session_id: None,
            traceparent: None,
        }
    }

    #[test]
    fn native_formats_are_preserved_verbatim_including_namespaced_non_json_grammars() {
        let declarations = vec![json!({"type":"namespace","name":"db","tools":[
            {"type":"custom","name":"sql","format":{"type":"grammar","syntax":"lark",
                "definition":"start: \"SELECT\" WS /[0-9]+/\n%import common.WS\n"}},
            {"type":"custom","name":"stamp","format":{"type":"grammar","syntax":"regex",
                "definition":"[0-9]{4}-[0-9]{2}-[0-9]{2}"}}
        ]})];
        let original = serde_json::to_vec(&declarations).unwrap();
        validate_provider(&config("native.example.com"), &declarations).unwrap();
        assert_eq!(serde_json::to_vec(&declarations).unwrap(), original);
    }

    #[test]
    fn validation_ability_does_not_depend_on_provider_brand() {
        let config = config("api.deepseek.com");
        for tool in [
            json!({"type":"custom","name":"sql"}),
            json!({"type":"custom","name":"apply_patch","format":{"type":"grammar","syntax":"regex","definition":".*"}}),
            json!({"type":"namespace","name":"db","tools":[{"type":"custom","name":"sql","format":{"type":"grammar","syntax":"lark","definition":"start: \"x\""}}]}),
        ] {
            validate_provider(&config, &[tool]).unwrap();
        }
        validate_provider(&config, &[json!({"type":"custom","name":"apply_patch"})]).unwrap();
        validate_provider(
            &config,
            &[json!({"type":"custom","name":"apply_patch","format":{"type":"text"}})],
        )
        .unwrap();
    }

    fn grammar(syntax: &str, definition: &str) -> Value {
        json!({"type":"custom","name":"raw","format":{"type":"grammar","syntax":syntax,"definition":definition}})
    }

    #[test]
    fn full_input_and_end_of_language_are_required_not_just_a_valid_prefix() {
        let tool = grammar("lark", "start: \"SELECT \" /[0-9]+/ \";\"");
        for valid in ["SELECT 0;", "SELECT 123;"] {
            validate_completed_input(&tool, valid).unwrap();
        }
        for invalid in ["", "SELECT ", "SELECT 1", "SELECT 1;DROP", "SELECT x;"] {
            assert!(
                validate_completed_input(&tool, invalid).is_err(),
                "{invalid:?}"
            );
        }
        let regex = grammar("regex", "[0-9]{4}-[0-9]{2}-[0-9]{2}");
        validate_completed_input(&regex, "2026-10-25").unwrap();
        for invalid in ["2026-10-", "2026-10-25extra", "prefix2026-10-25"] {
            assert!(validate_completed_input(&regex, invalid).is_err());
        }
        let anchored = grammar("regex", "^[0-9]+$");
        validate_completed_input(&anchored, "123").unwrap();
        assert!(validate_completed_input(&anchored, "123x").is_err());
    }

    #[test]
    fn unicode_bytes_and_ignored_whitespace_preserve_the_exact_language() {
        let tool = grammar("lark", "start: \"雪🙂\" \";\"\n%ignore /[ \\t]+/");
        validate_completed_input(&tool, "雪🙂  ;").unwrap();
        assert!(validate_completed_input(&tool, "雪😉;").is_err());
        let empty = grammar("regex", "a*");
        validate_completed_input(&empty, "").unwrap();
        assert!(validate_completed_input(&empty, "b").is_err());
    }

    #[test]
    fn invalid_or_excessive_grammar_and_input_are_never_accepted() {
        for definition in [
            "start: missing_rule".to_owned(),
            "start: /[/".to_owned(),
            format!(
                "start: {}",
                (0..9000)
                    .map(|index| format!("\"{}\"", char::from_u32(0x4e00 + index).unwrap()))
                    .collect::<Vec<_>>()
                    .join("|")
            ),
        ] {
            assert!(validate_tool(&grammar("lark", &definition)).is_err());
        }
        let text = json!({"type":"custom","name":"text"});
        assert!(validate_completed_input(&text, &"x".repeat(MAX_INPUT_BYTES + 1)).is_err());
        assert!(limits().max_lexer_states <= 4096);
        assert!(limits().max_grammar_size <= 8192);
        assert!(!limits().precompute_large_lexemes && !limits().verbose_errors);
    }

    proptest::proptest! {
        #[test]
        fn a_correct_complete_call_never_authorizes_a_truncated_or_extended_call(
            digits in "[0-9]{1,24}"
        ) {
            let tool = grammar("lark", "start: \"SELECT \" /[0-9]+/ \";\"");
            let value = format!("SELECT {digits};");
            proptest::prop_assert!(validate_completed_input(&tool, &value).is_ok());
            proptest::prop_assert!(validate_completed_input(&tool, &value[..value.len()-1]).is_err());
            let extended = format!("{value}DROP");
            proptest::prop_assert!(extended.starts_with("SELECT"));
            proptest::prop_assert!(validate_completed_input(&tool, &extended).is_err());
        }
    }

    #[test]
    fn function_bridge_is_one_raw_string_not_a_union_or_json_tool_substitute() {
        let tool = grammar("regex", "[a-z]+");
        let bridge = bridge_tool(&tool, "__agents_custom_123").unwrap();
        assert_eq!(
            bridge["parameters"]["properties"]
                .as_object()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(bridge["parameters"]["required"], json!(["input"]));
        assert_eq!(bridge["strict"], true);
        let raw = "SELECT  雪 ;\n";
        assert_eq!(
            restore_input(&json!({"input":raw}).to_string()).unwrap(),
            raw
        );
        assert!(restore_input(r#"{"input":"x","another":1}"#).is_err());
        assert!(bridge_tool(&tool, "caller-shadow").is_err());
    }

    #[test]
    fn model_token_directives_are_not_misinterpreted_as_byte_identifiers() {
        let tool = grammar("lark", "start: <[123]>");
        assert!(validate_tool(&tool).is_err());
        assert!(validate_completed_input(&tool, "{").is_err());
    }

    #[test]
    fn format_errors_are_bounded_and_do_not_repeat_caller_text() {
        let sentinel = "PRIVATE_GRAMMAR_SENTINEL";
        for value in [
            json!({"type":"grammar","syntax":sentinel,"definition":"x"}),
            json!({"type":"grammar","syntax":"regex","definition":sentinel,"extra":true}),
            json!({"type":"grammar","syntax":"regex","definition":"\0"}),
            json!({"type":"grammar","syntax":"lark","definition":" "}),
            json!({"type":"grammar","syntax":"lark","definition":"x".repeat(MAX_GRAMMAR_BYTES + 1)}),
            Value::Null,
        ] {
            let error = parse_format(Some(&value)).err().unwrap();
            assert!(!error.contains(sentinel));
            assert!(error.len() < 64);
        }
        assert!(matches!(parse_format(None).unwrap(), InputFormat::Text));
    }

    #[test]
    fn namespaces_share_a_bounded_declaration_budget() {
        let mut deep = json!({"type":"custom","name":"text"});
        for index in 0..=MAX_NAMESPACE_DEPTH {
            deep = json!({"type":"namespace","name":format!("n{index}"),"tools":[deep]});
        }
        assert_eq!(
            validate_provider(&config("native.example.com"), &[deep]).unwrap_err(),
            "custom_tool_declaration_limit"
        );
        let broad = vec![json!({"type":"custom","name":"text"}); MAX_DECLARATIONS + 1];
        assert!(validate_provider(&config("native.example.com"), &broad).is_err());
    }

    #[test]
    fn chat_and_lookalike_provider_origins_are_not_conflated() {
        let tool = json!({"type":"custom","name":"sql"});
        let mut chat = config("native.example.com");
        chat.protocol = ProviderProtocol::Openai;
        assert_eq!(
            validate_provider(&chat, &[tool.clone()]).unwrap_err(),
            "custom_tools_require_native_responses"
        );
        validate_provider(&config("api.deepseek.com.attacker.example"), &[tool]).unwrap();
    }
}
