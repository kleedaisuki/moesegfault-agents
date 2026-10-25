//! Retrieval policy shared by hosted file_search and Vector Store search.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use worker::{Error, Result};

const MAX_STORES: usize = 32;
const MAX_FILTER_NODES: usize = 64;
const MAX_FILTER_DEPTH: usize = 8;
const RRF_K: f64 = 60.0;

fn invalid(code: &str) -> Error {
    Error::RustError(code.into())
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileSearchOptions {
    pub vector_store_ids: Vec<String>,
    #[serde(default = "result_limit")]
    pub max_num_results: usize,
    #[serde(default)]
    pub filters: Option<Filter>,
    #[serde(default)]
    pub ranking_options: RankingOptions,
}

fn result_limit() -> usize {
    20
}

fn weight() -> f64 {
    1.0
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HybridWeights {
    #[serde(default = "weight")]
    pub embedding_weight: f64,
    #[serde(default = "weight")]
    pub text_weight: f64,
}

impl Default for HybridWeights {
    fn default() -> Self {
        Self {
            embedding_weight: 1.0,
            text_weight: 1.0,
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Ranker {
    None,
    #[default]
    Auto,
    #[serde(rename = "default-2024-11-15")]
    Default20241115,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RankingOptions {
    #[serde(default)]
    pub ranker: Ranker,
    #[serde(default)]
    pub score_threshold: f64,
    #[serde(default)]
    pub hybrid_search: HybridWeights,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Filter {
    Comparison(ComparisonFilter),
    Compound(CompoundFilter),
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComparisonFilter {
    #[serde(rename = "type")]
    pub operation: Comparison,
    pub key: String,
    pub value: Value,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Comparison {
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
    In,
    Nin,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompoundFilter {
    #[serde(rename = "type")]
    pub operation: Compound,
    pub filters: Vec<Filter>,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Compound {
    And,
    Or,
}

impl FileSearchOptions {
    pub fn validate(&self) -> Result<()> {
        if self.vector_store_ids.is_empty()
            || self.vector_store_ids.len() > MAX_STORES
            || !(1..=50).contains(&self.max_num_results)
            || self.vector_store_ids.iter().any(|id| !identifier(id))
            || self.vector_store_ids.iter().collect::<BTreeSet<_>>().len()
                != self.vector_store_ids.len()
        {
            return Err(invalid("invalid_file_search_options"));
        }
        self.ranking_options.validate()?;
        if let Some(filter) = &self.filters {
            let mut nodes = 0;
            filter.validate(0, &mut nodes)?;
        }
        Ok(())
    }
}

impl RankingOptions {
    pub fn validate(&self) -> Result<()> {
        let weights = &self.hybrid_search;
        if !self.score_threshold.is_finite()
            || !(0.0..=1.0).contains(&self.score_threshold)
            || !weights.embedding_weight.is_finite()
            || weights.embedding_weight < 0.0
            || !weights.text_weight.is_finite()
            || weights.text_weight < 0.0
            || weights.embedding_weight + weights.text_weight == 0.0
            || !(weights.embedding_weight + weights.text_weight).is_finite()
        {
            return Err(invalid("invalid_file_search_ranking"));
        }
        Ok(())
    }
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

fn scalar(value: &Value) -> bool {
    match value {
        Value::String(text) => text.len() <= 512,
        Value::Number(number) => number.as_f64().is_some_and(f64::is_finite),
        Value::Bool(_) => true,
        _ => false,
    }
}

impl Filter {
    fn validate(&self, depth: usize, nodes: &mut usize) -> Result<()> {
        *nodes += 1;
        if depth > MAX_FILTER_DEPTH || *nodes > MAX_FILTER_NODES {
            return Err(invalid("file_search_filter_too_complex"));
        }
        match self {
            Self::Comparison(filter) => filter.validate(),
            Self::Compound(filter) => {
                if filter.filters.is_empty() || filter.filters.len() > MAX_FILTER_NODES {
                    return Err(invalid("invalid_file_search_filter"));
                }
                for child in &filter.filters {
                    child.validate(depth + 1, nodes)?;
                }
                Ok(())
            }
        }
    }

    pub fn compile(&self) -> Result<Predicate> {
        let mut bindings = vec![];
        let sql = self.compile_into(&mut bindings)?;
        Ok(Predicate { sql, bindings })
    }

    pub fn compile_into(&self, bindings: &mut Vec<String>) -> Result<String> {
        self.validate(0, &mut 0)?;
        Ok(self.sql(bindings))
    }

    fn sql(&self, bindings: &mut Vec<String>) -> String {
        match self {
            Self::Comparison(filter) => filter.sql(bindings),
            Self::Compound(filter) => {
                let join = match filter.operation {
                    Compound::And => " AND ",
                    Compound::Or => " OR ",
                };
                let clauses = filter
                    .filters
                    .iter()
                    .map(|item| item.sql(bindings))
                    .collect::<Vec<_>>();
                format!("({})", clauses.join(join))
            }
        }
    }
}

impl ComparisonFilter {
    fn validate(&self) -> Result<()> {
        if self.key.is_empty()
            || self.key.chars().count() > 64
            || self.key.chars().any(char::is_control)
        {
            return Err(invalid("invalid_file_search_filter"));
        }
        match self.operation {
            Comparison::In | Comparison::Nin => {
                let Some(values) = self.value.as_array() else {
                    return Err(invalid("invalid_file_search_filter"));
                };
                if values.is_empty()
                    || values.len() > 100
                    || values.iter().any(|value| !scalar(value))
                {
                    return Err(invalid("invalid_file_search_filter"));
                }
            }
            _ if !scalar(&self.value) => return Err(invalid("invalid_file_search_filter")),
            Comparison::Gt | Comparison::Gte | Comparison::Lt | Comparison::Lte
                if self.value.is_boolean() =>
            {
                return Err(invalid("invalid_file_search_filter"));
            }
            _ => {}
        }
        Ok(())
    }

    fn sql(&self, bindings: &mut Vec<String>) -> String {
        bindings.push(self.key.clone());
        let key = format!("?{}", bindings.len());
        let predicate = match self.operation {
            Comparison::In | Comparison::Nin => {
                let items = self
                    .value
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|value| scalar_predicate(value, "=", bindings))
                    .collect::<Vec<_>>()
                    .join(" OR ");
                if matches!(self.operation, Comparison::Nin) {
                    format!("NOT ({items})")
                } else {
                    format!("({items})")
                }
            }
            operation => {
                let operator = match operation {
                    Comparison::Eq => "=",
                    Comparison::Ne => "!=",
                    Comparison::Gt => ">",
                    Comparison::Gte => ">=",
                    Comparison::Lt => "<",
                    Comparison::Lte => "<=",
                    _ => unreachable!(),
                };
                scalar_predicate(&self.value, operator, bindings)
            }
        };
        // json_each compares keys as data, including dots/quotes. No attribute
        // name is interpolated into SQL or an ad-hoc JSON-path expression.
        format!(
            "EXISTS (SELECT 1 FROM json_each(vf.attributes_json) AS attr WHERE attr.key={key} AND {predicate})"
        )
    }
}

fn scalar_predicate(value: &Value, operator: &str, bindings: &mut Vec<String>) -> String {
    bindings.push(value.to_string());
    let parameter = format!("?{}", bindings.len());
    let compatible = match value {
        Value::Number(_) => "attr.type IN ('integer','real')",
        Value::String(_) => "attr.type='text'",
        Value::Bool(_) => "attr.type IN ('true','false')",
        _ => unreachable!(),
    };
    format!("({compatible} AND attr.value {operator} json_extract({parameter},'$'))")
}

pub struct Predicate {
    pub sql: String,
    pub bindings: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Hit {
    pub chunk_id: String,
    pub file_id: String,
    pub filename: String,
    pub attributes: BTreeMap<String, Value>,
    pub text: String,
    pub score: f64,
}

impl Hit {
    pub fn tool_result(&self) -> Value {
        json!({"file_id":self.file_id,"filename":self.filename,"attributes":self.attributes,
            "text":self.text,"score":self.score})
    }

    pub fn vector_result(&self) -> Value {
        json!({"file_id":self.file_id,"filename":self.filename,"attributes":self.attributes,
            "content":[{"type":"text","text":self.text}],"score":self.score})
    }
}

/// Weighted reciprocal-rank fusion uses real lexical/vector order, never
/// synthetic vectors or a claim that a BM25 number is cosine similarity.
pub fn fuse(
    lexical: &[String],
    semantic: &[String],
    ranking: &RankingOptions,
    limit: usize,
) -> Result<Vec<(String, f64)>> {
    ranking.validate()?;
    let mut scores = BTreeMap::<String, f64>::new();
    let weights = &ranking.hybrid_search;
    for (ids, weight) in [
        (lexical, weights.text_weight),
        (semantic, weights.embedding_weight),
    ] {
        if weight == 0.0 {
            continue;
        }
        let mut seen = BTreeSet::new();
        for (index, id) in ids.iter().enumerate() {
            if seen.insert(id) {
                *scores.entry(id.clone()).or_default() += weight / (RRF_K + index as f64 + 1.0);
            }
        }
    }
    let maximum = (weights.text_weight + weights.embedding_weight) / (RRF_K + 1.0);
    let mut ranked = scores
        .into_iter()
        .map(|(id, score)| (id, score / maximum))
        .collect::<Vec<_>>();
    ranked.retain(|(_, score)| *score >= ranking.score_threshold);
    ranked.sort_by(|(left_id, left), (right_id, right)| {
        right.total_cmp(left).then(left_id.cmp(right_id))
    });
    ranked.truncate(limit.min(50));
    Ok(ranked)
}

pub fn lexical_query(query: &str) -> Result<String> {
    if query.trim().is_empty() || query.len() > 8192 || query.contains('\0') {
        return Err(invalid("invalid_file_search_query"));
    }
    let terms = query
        .split_whitespace()
        .take(128)
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect::<Vec<_>>();
    Ok(terms.join(" OR "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_options_and_filters_are_bounded_and_not_silently_ignored() {
        let options: FileSearchOptions =
            serde_json::from_value(json!({"vector_store_ids":["vs_1"],
            "filters":{"type":"and","filters":[{"type":"eq","key":"a.b\"key","value":"x' OR 1=1"},
                {"type":"gte","key":"date","value":100}]}}))
            .unwrap();
        options.validate().unwrap();
        let predicate = options.filters.unwrap().compile().unwrap();
        assert!(!predicate.sql.contains("a.b") && !predicate.sql.contains("1=1"));
        assert_eq!(predicate.bindings[0], "a.b\"key");
        assert_eq!(predicate.bindings[1], "\"x' OR 1=1\"");
        assert!(
            serde_json::from_value::<FileSearchOptions>(
                json!({"vector_store_ids":["vs_1"],"ignored":true})
            )
            .is_err()
        );
    }

    #[test]
    fn rank_fusion_honors_real_weights_thresholds_and_stable_identity() {
        let ranking = RankingOptions {
            hybrid_search: HybridWeights {
                embedding_weight: 0.0,
                text_weight: 1.0,
            },
            ..Default::default()
        };
        let result = fuse(
            &["lexical".into(), "both".into()],
            &["semantic".into(), "both".into()],
            &ranking,
            10,
        )
        .unwrap();
        assert_eq!(result[0], ("lexical".into(), 1.0));
        assert!(result.iter().any(|(id, _)| id == "both"));
        assert!(!result.iter().any(|(id, _)| id == "semantic"));
        let invalid = RankingOptions {
            hybrid_search: HybridWeights {
                embedding_weight: f64::NAN,
                text_weight: 1.0,
            },
            ..Default::default()
        };
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn query_text_cannot_inject_fts_operators() {
        assert_eq!(
            lexical_query("hello OR world\"").unwrap(),
            "\"hello\" OR \"OR\" OR \"world\"\"\""
        );
        assert!(lexical_query("").is_err());
    }
}
