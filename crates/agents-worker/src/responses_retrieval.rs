//! Retrieval options are observation policy, not persisted generation policy.

use super::ResponseRequest;

#[derive(Clone, Debug)]
pub struct ResponseProjection {
    pub include: Vec<String>,
    pub include_obfuscation: bool,
}

impl Default for ResponseProjection {
    fn default() -> Self {
        Self {
            include: vec![],
            include_obfuscation: true,
        }
    }
}

impl ResponseProjection {
    pub fn for_request(request: &ResponseRequest) -> Self {
        Self {
            include: request.include.clone(),
            include_obfuscation: request
                .stream_options
                .as_ref()
                .and_then(|options| options.get("include_obfuscation"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct RetrieveOptions {
    pub stream: bool,
    pub after: Option<u64>,
    pub projection: ResponseProjection,
    obfuscation_supplied: bool,
}

impl RetrieveOptions {
    pub fn parse(url: &url::Url, private_events: bool) -> Result<Self, &'static str> {
        let mut options = Self {
            stream: private_events,
            ..Self::default()
        };
        let mut seen = std::collections::BTreeSet::new();
        for (key, value) in url.query_pairs() {
            if matches!(key.as_ref(), "include" | "include[]") {
                if !super::controls::valid_include(&value) {
                    return Err("Unknown Responses include projection");
                }
                if !options
                    .projection
                    .include
                    .iter()
                    .any(|item| item == value.as_ref())
                {
                    options.projection.include.push(value.into_owned());
                }
                continue;
            }
            if !seen.insert(key.to_string()) {
                return Err("Repeated Responses query parameter");
            }
            match key.as_ref() {
                "stream" if !private_events => options.stream = boolean(&value)?,
                "starting_after" if !private_events => options.after = Some(sequence(&value)?),
                "after" if private_events => options.after = Some(sequence(&value)?),
                "include_obfuscation" => {
                    options.projection.include_obfuscation = boolean(&value)?;
                    options.obfuscation_supplied = true;
                }
                _ => return Err("Unsupported Responses query parameter"),
            }
        }
        if options.after.is_some() && !options.stream {
            return Err("starting_after requires stream=true");
        }
        Ok(options)
    }

    pub fn private_query(&self) -> String {
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        if let Some(after) = self.after {
            query.append_pair("after", &after.to_string());
        }
        for include in &self.projection.include {
            query.append_pair("include[]", include);
        }
        if self.obfuscation_supplied {
            query.append_pair(
                "include_obfuscation",
                if self.projection.include_obfuscation {
                    "true"
                } else {
                    "false"
                },
            );
        }
        let query = query.finish();
        if query.is_empty() {
            query
        } else {
            format!("?{query}")
        }
    }

    pub fn private_suffix(&self) -> String {
        format!(
            "{}{}",
            if self.stream { "/events" } else { "" },
            self.private_query()
        )
    }
}

fn boolean(value: &str) -> Result<bool, &'static str> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err("Responses query boolean must be true or false"),
    }
}

fn sequence(value: &str) -> Result<u64, &'static str> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("Invalid response event cursor");
    }
    value.parse().map_err(|_| "Invalid response event cursor")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdk_arrays_and_obfuscation_survive_private_forwarding() {
        let url = url::Url::parse("https://agents.example/v1/responses/resp_1?stream=true&starting_after=12&include%5B%5D=message.output_text.logprobs&include%5B%5D=code_interpreter_call.outputs&include_obfuscation=false").unwrap();
        let options = RetrieveOptions::parse(&url, false).unwrap();
        let internal = url::Url::parse(&format!(
            "https://session.example/responses/resp_1{}",
            options.private_suffix()
        ))
        .unwrap();
        let forwarded = RetrieveOptions::parse(&internal, true).unwrap();
        assert_eq!(forwarded.after, Some(12));
        assert_eq!(forwarded.projection.include, options.projection.include);
        assert!(!forwarded.projection.include_obfuscation);
    }

    #[test]
    fn retrieval_options_are_strict_and_do_not_mutate_generation_policy() {
        for query in [
            "?stream=yes",
            "?stream=true&stream=false",
            "?starting_after=1",
            "?stream=true&starting_after=+1",
            "?stream=true&starting_after=18446744073709551616",
            "?include[]=secret",
            "?include_obfuscation=no",
            "?after=1",
        ] {
            let url = url::Url::parse(&format!(
                "https://agents.example/v1/responses/resp_1{query}"
            ))
            .unwrap();
            assert!(RetrieveOptions::parse(&url, false).is_err(), "{query}");
        }
        let url = url::Url::parse("https://agents.example/v1/responses/resp_1?include[]=reasoning.encrypted_content&include[]=reasoning.encrypted_content").unwrap();
        let options = RetrieveOptions::parse(&url, false).unwrap();
        assert_eq!(
            options.projection.include,
            vec!["reasoning.encrypted_content"]
        );
        assert!(options.projection.include_obfuscation);
    }
}
