//! OpenAI Vector Store HTTP boundary, using the same owned index as file_search.

use serde::Deserialize;
use serde_json::{Value, json};
use worker::{D1Result, Env, Error, Method, Request, Response, Result};

use crate::{
    conversation_meter::ConversationOperation,
    file_search::{FileSearchOptions, RankingOptions},
    resources::ResourceMeter,
    routes::{self, RouteContext},
    util, vector_store_routes as stores,
};

fn fail(code: &str) -> Error {
    Error::RustError(code.into())
}
fn invalid() -> Result<Response> {
    util::json_error(
        400,
        "invalid_vector_store_request",
        "Invalid Vector Store request",
    )
}
fn missing() -> Result<Response> {
    util::json_error(
        404,
        "vector_store_not_found",
        "Vector Store or file not found",
    )
}

async fn query(
    env: &Env,
    meter: &ResourceMeter<'_>,
    sql: &str,
    bindings: &[Value],
) -> Result<D1Result> {
    let event = format!("vs:{}", uuid::Uuid::new_v4().simple());
    let bindings = bindings
        .iter()
        .map(|value| match value {
            Value::Null => Ok(wasm_bindgen::JsValue::NULL),
            Value::String(value) => Ok(value.as_str().into()),
            Value::Number(value) => value
                .as_f64()
                .map(wasm_bindgen::JsValue::from_f64)
                .ok_or_else(|| fail("invalid_vector_store_binding")),
            _ => Err(fail("invalid_vector_store_binding")),
        })
        .collect::<Result<Vec<_>>>()?;
    meter.require_d1(&event).await?;
    let result = env.d1("DB")?.prepare(sql).bind(&bindings)?.all().await?;
    meter.record_d1(&event, &result).await?;
    Ok(result)
}

pub(crate) async fn handle(req: Request, env: &RouteContext<'_>, owner: &str) -> Result<Response> {
    if let Some(response) = cancel_unfunded(&req, env, owner).await? {
        return Ok(response);
    }
    let operation = ConversationOperation::account(env, owner).await?;
    let meter = ResourceMeter::account(env, owner, operation.id())?;
    let result = match dispatch(req, env, owner, &meter).await {
        Err(Error::RustError(code))
            if code.starts_with("invalid_vector_store")
                || code.starts_with("invalid_file_search")
                || code == "vector_store_source_unavailable_or_limit"
                || code == "requested_file_search_ranker_unavailable"
                || code == "file_search_eligible_file_budget_exceeded" =>
        {
            util::json_error(400, &code, "Invalid Vector Store request")
        }
        Err(Error::RustError(code)) if code == "vector_store_not_found" => missing(),
        Err(Error::RustError(code)) if code == "vector_store_limit_reached" => {
            util::json_error(429, &code, "Vector Store limit reached")
        }
        Err(Error::RustError(code)) if code == "file_search_store_requires_reindex" => {
            util::json_error(
                409,
                &code,
                "The index requires its pinned embedding profile",
            )
        }
        Err(Error::RustError(code))
            if code.starts_with("file_search_")
                && (code.ends_with("not_configured")
                    || code.ends_with("binding_missing")
                    || code.ends_with("method_missing")) =>
        {
            util::json_error(503, &code, "File search service is not configured")
        }
        result => result,
    };
    operation.finish_result(env, owner, result, false).await
}

async fn cancel_unfunded(req: &Request, env: &Env, owner: &str) -> Result<Option<Response>> {
    if req.method() != Method::Delete {
        return Ok(None);
    }
    let path = req.path();
    let parts = path.trim_matches('/').split('/').collect::<Vec<_>>();
    let ["v1", "vector_stores", store, "files", file] = parts.as_slice() else {
        return Ok(None);
    };
    // Stopping an already unfunded job cannot require more credit. This bounded,
    // authenticated control-plane tombstone is platform overhead; physical GC
    // remains separately funded and unknown provider holds are never released.
    let rows=env.d1("DB")?.prepare("UPDATE vector_store_files SET status='cancelled',removed_at=COALESCE(removed_at,CAST(?4 AS INTEGER)),lease_id=NULL,lease_until=0 WHERE account_id=?1 AND vector_store_id=?2 AND file_id=?3 AND funding_blocked=1 AND status IN ('in_progress','cancelled') AND EXISTS(SELECT 1 FROM vector_stores vs WHERE vs.id=?2 AND vs.account_id=?1) RETURNING file_id")
        .bind(&[owner.into(),(*store).into(),(*file).into(),util::now_ms().to_string().into()])?.all().await?;
    if rows.results::<Value>()?.is_empty() {
        return Ok(None);
    }
    Response::from_json(&json!({"id":file,"object":"vector_store.file.deleted","deleted":true}))
        .map(Some)
}

async fn dispatch(
    mut req: Request,
    env: &RouteContext<'_>,
    owner: &str,
    meter: &ResourceMeter<'_>,
) -> Result<Response> {
    let path = req.path();
    let parts = path.trim_matches('/').split('/').collect::<Vec<_>>();
    let method = req.method();
    let url = req.url()?;
    let limit = url
        .query_pairs()
        .find(|(key, _)| key == "limit")
        .map(|(_, value)| value.parse::<u16>())
        .transpose()
        .map_err(|_| fail("invalid_vector_store_limit"))?
        .unwrap_or(20);
    if !(1..=100).contains(&limit) {
        return invalid();
    }
    if parts.as_slice() == ["v1", "vector_stores"] {
        return match method {
            Method::Get => {
                let page = Page::parse(&req)?;
                let mut bindings = vec![json!(owner)];
                let cursor = page.cursor(
                    "vs.created_at",
                    "vs.id",
                    "vector_stores",
                    "id",
                    &mut bindings,
                    None,
                );
                bindings.push(json!(limit + 1));
                let sql = format!(
                    "{} WHERE vs.account_id=?1 AND vs.status NOT IN ('deleted','deleting') AND ({cursor}) GROUP BY vs.id ORDER BY vs.created_at {},vs.id {} LIMIT ?{}",
                    stores::STORE_SELECT,
                    page.order(),
                    page.order(),
                    bindings.len()
                );
                let rows = query(env, meter, &sql, &bindings)
                    .await?
                    .results::<stores::StoreRow>()?;
                let mut values = rows
                    .iter()
                    .map(stores::StoreRow::public)
                    .collect::<Result<Vec<_>>>()?;
                let more = values.len() > usize::from(limit);
                values.truncate(usize::from(limit));
                list("list", values, more)
            }
            Method::Post => {
                let mut body: Value = match routes::body(&mut req).await {
                    Ok(body) => body,
                    Err(response) => return Ok(response),
                };
                let files = take_ids(&mut body, "file_ids")?;
                let chunking = body
                    .as_object_mut()
                    .and_then(|fields| fields.remove("chunking_strategy"));
                let input: stores::CreateStore = serde_json::from_value(body)
                    .map_err(|_| fail("invalid_vector_store_request"))?;
                if !files.is_empty() {
                    crate::file_ingest::preflight(&crate::file_ingest::profile(env)?, meter)?;
                }
                let store =
                    stores::create_store(env, owner, &input, 100, meter, "vs:create").await?;
                if !files.is_empty() {
                    let strategy = chunking
                        .map(serde_json::from_value)
                        .transpose()?
                        .unwrap_or_default();
                    stores::attach_batch(
                        env,
                        owner,
                        &store.id,
                        &files,
                        &Default::default(),
                        &strategy,
                        meter,
                        "vs:create:files",
                    )
                    .await?;
                }
                store_response(env, owner, &store.id, meter).await
            }
            _ => method_not_allowed(),
        };
    }
    let Some(id) = parts.get(2).copied() else {
        return missing();
    };
    if !id.starts_with("vs_") || id.len() > 100 {
        return missing();
    }
    if parts.len() == 3 {
        return match method {
            Method::Get => store_response(env, owner, id, meter).await,
            Method::Post => {
                let body: Value = match routes::body(&mut req).await {
                    Ok(body) => body,
                    Err(response) => return Ok(response),
                };
                update_store(env, owner, id, &body, meter).await?;
                store_response(env, owner, id, meter).await
            }
            Method::Delete => {
                if stores::delete_store(env, owner, id, meter, "vs:delete").await? {
                    Response::from_json(
                        &json!({"id":id,"object":"vector_store.deleted","deleted":true}),
                    )
                } else {
                    missing()
                }
            }
            _ => method_not_allowed(),
        };
    }
    if stores::get_store(env, owner, id, meter, "vs:owner")
        .await?
        .is_none()
    {
        return missing();
    }
    match parts.get(3).copied() {
        Some("search") if parts.len() == 4 && method == Method::Post => {
            let body: Search = match routes::body(&mut req).await {
                Ok(body) => body,
                Err(response) => return Ok(response),
            };
            if body.rewrite_query {
                return util::json_error(
                    400,
                    "query_rewriting_unavailable",
                    "Query rewriting requires an explicitly configured inference service",
                );
            }
            let queries = match body.query {
                Value::String(query) => vec![query],
                Value::Array(queries) => queries
                    .into_iter()
                    .map(|v| {
                        v.as_str()
                            .map(str::to_owned)
                            .ok_or_else(|| fail("invalid_file_search_queries"))
                    })
                    .collect::<Result<_>>()?,
                _ => return invalid(),
            };
            let options = FileSearchOptions {
                vector_store_ids: vec![id.into()],
                max_num_results: body.max_num_results,
                filters: body.filters,
                ranking_options: body.ranking_options,
            };
            let hits = crate::file_ingest::search_queries(
                env,
                owner,
                &queries,
                &options,
                meter,
                "vs:search",
            )
            .await?;

            Response::from_json(
                &json!({"object":"vector_store.search_results.page","search_query":queries,"data":hits.iter().map(|hit|hit.vector_result()).collect::<Vec<_>>(),"has_more":false,"next_page":null}),
            )
        }
        Some("files") => file_route(req, env, owner, id, &parts[4..], limit, meter).await,
        Some("file_batches") => batch_route(req, env, owner, id, &parts[4..], meter).await,
        _ => missing(),
    }
}

struct Page {
    ascending: bool,
    after: Option<String>,
    before: Option<String>,
}

impl Page {
    fn parse(req: &Request) -> Result<Self> {
        let mut page = Self {
            ascending: false,
            after: None,
            before: None,
        };
        let mut seen = std::collections::BTreeSet::new();
        for (key, value) in req.url()?.query_pairs() {
            if !seen.insert(key.to_string()) {
                return Err(fail("invalid_vector_store_pagination"));
            }
            match key.as_ref() {
                "limit" | "filter" => {}
                "order" if value == "asc" => page.ascending = true,
                "order" if value == "desc" => page.ascending = false,
                "after" if !value.is_empty() && value.len() <= 100 => {
                    page.after = Some(value.into_owned())
                }
                "before" if !value.is_empty() && value.len() <= 100 => {
                    page.before = Some(value.into_owned())
                }
                _ => return Err(fail("invalid_vector_store_pagination")),
            }
        }
        if page.after.is_some() && page.before.is_some() {
            return Err(fail("invalid_vector_store_pagination"));
        }
        Ok(page)
    }

    fn order(&self) -> &'static str {
        if self.ascending { "ASC" } else { "DESC" }
    }

    fn cursor(
        &self,
        time: &str,
        id: &str,
        table: &str,
        cursor_id: &str,
        bindings: &mut Vec<Value>,
        store: Option<&str>,
    ) -> String {
        let (cursor, after) = match (&self.after, &self.before) {
            (Some(cursor), _) => (cursor, true),
            (_, Some(cursor)) => (cursor, false),
            _ => return "1".into(),
        };
        bindings.push(json!(cursor));
        let slot = bindings.len();
        let compare = if self.ascending == after { ">" } else { "<" };
        let scope = store
            .map(|store| {
                bindings.push(json!(store));
                format!("AND vector_store_id=?{}", bindings.len())
            })
            .unwrap_or_default();
        format!(
            "({time},{id}) {compare} (SELECT created_at,{cursor_id} FROM {table} WHERE account_id=?1 AND {cursor_id}=?{slot} {scope})"
        )
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Search {
    query: Value,
    #[serde(default = "search_limit")]
    max_num_results: usize,
    #[serde(default)]
    filters: Option<crate::file_search::Filter>,
    #[serde(default)]
    ranking_options: RankingOptions,
    #[serde(default)]
    rewrite_query: bool,
}
fn search_limit() -> usize {
    20
}

fn take_ids(body: &mut Value, key: &str) -> Result<Vec<String>> {
    let values = body
        .as_object_mut()
        .and_then(|fields| fields.remove(key))
        .unwrap_or_else(|| json!([]));
    let ids: Vec<String> =
        serde_json::from_value(values).map_err(|_| fail("invalid_vector_store_file_ids"))?;
    if ids.len() > 500
        || ids.iter().any(|id| id.is_empty() || id.len() > 100)
        || ids.iter().collect::<std::collections::BTreeSet<_>>().len() != ids.len()
    {
        return Err(fail("invalid_vector_store_file_ids"));
    }
    Ok(ids)
}

async fn store_response(
    env: &RouteContext<'_>,
    owner: &str,
    id: &str,
    meter: &ResourceMeter<'_>,
) -> Result<Response> {
    match stores::get_store(env, owner, id, meter, "vs:get").await? {
        Some(store) => {
            if store.in_progress > 0 {
                wake(env, owner, id, None);
            }
            Response::from_json(&store.public()?)
        }
        None => missing(),
    }
}

fn list(object: &str, values: Vec<Value>, has_more: bool) -> Result<Response> {
    Response::from_json(
        &json!({"object":object,"first_id":values.first().and_then(|v|v.get("id")),"last_id":values.last().and_then(|v|v.get("id")),"data":values,"has_more":has_more}),
    )
}
fn method_not_allowed() -> Result<Response> {
    util::json_error(405, "method_not_allowed", "Method not allowed")
}

async fn update_store(
    env: &Env,
    owner: &str,
    id: &str,
    body: &Value,
    meter: &ResourceMeter<'_>,
) -> Result<()> {
    let input: stores::CreateStore =
        serde_json::from_value(body.clone()).map_err(|_| fail("invalid_vector_store_request"))?;
    stores::validate_update(&input)?;
    let fields = body
        .as_object()
        .ok_or_else(|| fail("invalid_vector_store_request"))?;
    for key in fields.keys() {
        let (column, value) = match key.as_str() {
            "name" => (
                "name",
                input.name.clone().map(Value::String).unwrap_or(Value::Null),
            ),
            "metadata" => (
                "metadata_json",
                json!(serde_json::to_string(&input.metadata)?),
            ),
            "expires_after" => (
                "expires_after_days",
                input
                    .expires_after
                    .as_ref()
                    .map(|v| json!(v.days))
                    .unwrap_or(Value::Null),
            ),
            _ => return Err(fail("invalid_vector_store_request")),
        };
        query(env,meter,&format!("UPDATE vector_stores SET {column}=?3 WHERE id=?1 AND account_id=?2 AND status='completed'"),&[json!(id),json!(owner),value]).await?;
    }
    Ok(())
}

async fn file_route(
    mut req: Request,
    env: &RouteContext<'_>,
    owner: &str,
    store: &str,
    tail: &[&str],
    limit: u16,
    meter: &ResourceMeter<'_>,
) -> Result<Response> {
    let method = req.method();
    if tail.is_empty() {
        return match method {
            Method::Post => {
                let input: stores::AttachFile = match routes::body(&mut req).await {
                    Ok(input) => input,
                    Err(response) => return Ok(response),
                };
                stores::attach_file(env, owner, store, &input, 10000, meter, "vs:attach").await?;
                file_response(env, owner, store, &input.file_id, meter).await
            }
            Method::Get => {
                let rows = list_members(&req, env, owner, store, None, limit, meter).await?;
                let more = rows.len() > usize::from(limit);
                list(
                    "list",
                    rows.iter()
                        .take(usize::from(limit))
                        .map(file_public)
                        .collect::<Result<_>>()?,
                    more,
                )
            }
            _ => method_not_allowed(),
        };
    }
    let id = tail[0];
    if tail.len() == 2 && tail[1] == "retry" && method == Method::Post {
        let body: Value = match routes::body(&mut req).await {
            Ok(body) => body,
            Err(response) => return Ok(response),
        };
        if body.as_object().is_none_or(|fields| !fields.is_empty()) {
            return invalid();
        }
        let result=query(env,meter,"UPDATE vector_store_files SET funding_blocked=0,funding_required_microusd=0,last_error_json=NULL,retry_at=0 WHERE account_id=?1 AND vector_store_id=?2 AND file_id=?3 AND status='in_progress' AND funding_blocked=1 AND removed_at IS NULL AND EXISTS(SELECT 1 FROM usage_accounts ua WHERE ua.account_id=?1 AND ua.budget_microusd-ua.spent_microusd-ua.reserved_microusd>=funding_required_microusd) AND NOT EXISTS(SELECT 1 FROM file_search_chunks c WHERE c.account_id=?1 AND c.vector_store_id=?2 AND c.file_id=?3 AND c.generation=vector_store_files.generation AND c.vector_status='embedding') AND NOT EXISTS(SELECT 1 FROM file_search_funding f JOIN resource_observations o ON o.run_id=f.id AND o.account_id=f.account_id WHERE f.account_id=?1 AND f.vector_store_id=?2 AND f.file_id=?3 AND f.generation=vector_store_files.generation AND f.operation='convert' AND o.status='pending') RETURNING file_id",&[json!(owner),json!(store),json!(id)]).await?;
        if result.results::<Value>()?.is_empty() {
            return util::json_error(
                409,
                "file_search_retry_not_admitted",
                "Retry requires an owned paused membership, sufficient resource credit and no unknown submitted conversion or embedding",
            );
        }
        return file_response(env, owner, store, id, meter).await;
    }
    if tail.len() == 2 && tail[1] == "content" && method == Method::Get {
        let rows=query(env,meter,"SELECT c.text FROM file_search_chunks c JOIN vector_store_files vf ON vf.vector_store_id=c.vector_store_id AND vf.file_id=c.file_id JOIN space_files sf ON sf.id=c.file_id WHERE c.account_id=?1 AND c.vector_store_id=?2 AND c.file_id=?3 AND vf.account_id=?1 AND vf.status='completed' AND vf.generation=c.generation AND sf.account_id=?1 AND sf.status='processed' AND sf.sha256=c.file_sha256 ORDER BY c.ordinal LIMIT 100",&[json!(owner),json!(store),json!(id)]).await?.results::<Value>()?;
        return Response::from_json(
            &json!({"object":"vector_store.file_content.page","data":rows.into_iter().map(|v|json!({"type":"text","text":v["text"]})).collect::<Vec<_>>(),"has_more":false,"next_page":null}),
        );
    }
    if tail.len() != 1 {
        return missing();
    }
    match method {
        Method::Get => file_response(env, owner, store, id, meter).await,
        Method::Delete => {
            if stores::detach_file(env, owner, store, id, meter, "vs:detach").await? {
                Response::from_json(
                    &json!({"id":id,"object":"vector_store.file.deleted","deleted":true}),
                )
            } else {
                missing()
            }
        }
        Method::Post => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Update {
                attributes: std::collections::BTreeMap<String, Value>,
            }
            let input: Update = match routes::body(&mut req).await {
                Ok(input) => input,
                Err(response) => return Ok(response),
            };
            stores::validate_attributes(&input.attributes)?;
            query(env,meter,"UPDATE vector_store_files SET attributes_json=?4,updated_at=CAST(?5 AS INTEGER) WHERE account_id=?1 AND vector_store_id=?2 AND file_id=?3 AND removed_at IS NULL",&[json!(owner),json!(store),json!(id),json!(serde_json::to_string(&input.attributes)?),json!(util::now_ms())]).await?;
            file_response(env, owner, store, id, meter).await
        }
        _ => method_not_allowed(),
    }
}

async fn list_members(
    req: &Request,
    env: &Env,
    owner: &str,
    store: &str,
    batch: Option<&str>,
    limit: u16,
    meter: &ResourceMeter<'_>,
) -> Result<Vec<Value>> {
    let page = Page::parse(req)?;
    let mut bindings = vec![json!(owner), json!(store)];
    let cursor = page.cursor(
        "vf.created_at",
        "vf.file_id",
        "vector_store_files",
        "file_id",
        &mut bindings,
        Some(store),
    );
    let mut filter = "1".to_string();
    if let Some((_, status)) = req.url()?.query_pairs().find(|(key, _)| key == "filter") {
        if !matches!(
            status.as_ref(),
            "in_progress" | "completed" | "failed" | "cancelled"
        ) {
            return Err(fail("invalid_vector_store_status_filter"));
        }
        bindings.push(json!(status));
        filter = format!("vf.status=?{}", bindings.len());
    }
    if let Some(batch) = batch {
        bindings.push(json!(batch));
        filter = format!("({filter}) AND vf.batch_id=?{}", bindings.len());
    }
    bindings.push(json!(limit + 1));
    let sql = format!(
        "SELECT vf.*,sf.filename FROM vector_store_files vf JOIN space_files sf ON sf.id=vf.file_id AND sf.account_id=vf.account_id WHERE vf.account_id=?1 AND vf.vector_store_id=?2 AND vf.removed_at IS NULL AND ({cursor}) AND ({filter}) ORDER BY vf.created_at {},vf.file_id {} LIMIT ?{}",
        page.order(),
        page.order(),
        bindings.len()
    );
    query(env, meter, &sql, &bindings).await?.results()
}

async fn file_response(
    env: &RouteContext<'_>,
    owner: &str,
    store: &str,
    id: &str,
    meter: &ResourceMeter<'_>,
) -> Result<Response> {
    let row=query(env,meter,"SELECT vf.*,sf.filename FROM vector_store_files vf JOIN space_files sf ON sf.id=vf.file_id AND sf.account_id=vf.account_id WHERE vf.account_id=?1 AND vf.vector_store_id=?2 AND vf.file_id=?3 AND vf.removed_at IS NULL",&[json!(owner),json!(store),json!(id)]).await?.results::<Value>()?.into_iter().next();
    match row {
        Some(row) => {
            if row["status"] == "in_progress" {
                wake(env, owner, store, Some(id));
            }
            Response::from_json(&file_public(&row)?)
        }
        None => missing(),
    }
}

fn file_public(row: &Value) -> Result<Value> {
    let attributes: Value = serde_json::from_str(row["attributes_json"].as_str().unwrap_or("{}"))?;
    let chunking: Value = serde_json::from_str(
        row["chunking_json"]
            .as_str()
            .unwrap_or("{\"type\":\"auto\"}"),
    )?;
    let error: Value = row["last_error_json"]
        .as_str()
        .map(serde_json::from_str)
        .transpose()?
        .unwrap_or(Value::Null);
    Ok(
        json!({"id":row["file_id"],"object":"vector_store.file","vector_store_id":row["vector_store_id"],"created_at":row["created_at"].as_u64().unwrap_or(0)/1000,"usage_bytes":row["usage_bytes"],"status":row["status"],"last_error":error,"attributes":attributes,"chunking_strategy":chunking}),
    )
}

async fn batch_route(
    mut req: Request,
    env: &RouteContext<'_>,
    owner: &str,
    store: &str,
    tail: &[&str],
    meter: &ResourceMeter<'_>,
) -> Result<Response> {
    let method = req.method();
    if tail.is_empty() && method == Method::Post {
        let mut body: Value = match routes::body(&mut req).await {
            Ok(body) => body,
            Err(response) => return Ok(response),
        };
        let files = take_ids(&mut body, "file_ids")?;
        if files.is_empty() {
            return invalid();
        }
        let chunking = body
            .as_object_mut()
            .and_then(|fields| fields.remove("chunking_strategy"));
        let attributes = body
            .as_object_mut()
            .and_then(|fields| fields.remove("attributes"))
            .unwrap_or_else(|| json!({}));
        if body.as_object().is_none_or(|fields| !fields.is_empty()) {
            return invalid();
        }
        let attributes: std::collections::BTreeMap<String, Value> =
            serde_json::from_value(attributes)?;
        stores::validate_attributes(&attributes)?;
        let strategy = chunking
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_default();
        let batch = stores::attach_batch(
            env,
            owner,
            store,
            &files,
            &attributes,
            &strategy,
            meter,
            "vs:batch",
        )
        .await?;
        return batch_response(env, owner, store, &batch, meter).await;
    }
    let Some(batch) = tail.first().copied() else {
        return method_not_allowed();
    };
    if tail.len() == 2 && tail[1] == "files" && method == Method::Get {
        let limit = req
            .url()?
            .query_pairs()
            .find(|(key, _)| key == "limit")
            .map(|(_, value)| value.parse::<u16>())
            .transpose()
            .map_err(|_| fail("invalid_vector_store_limit"))?
            .unwrap_or(20);
        let exists=query(env,meter,"SELECT id FROM vector_store_file_batches WHERE id=?1 AND account_id=?2 AND vector_store_id=?3",&[json!(batch),json!(owner),json!(store)]).await?.results::<Value>()?;
        if exists.is_empty() {
            return missing();
        }
        let rows = list_members(&req, env, owner, store, Some(batch), limit, meter).await?;
        let more = rows.len() > usize::from(limit);
        return list(
            "list",
            rows.iter()
                .take(usize::from(limit))
                .map(file_public)
                .collect::<Result<_>>()?,
            more,
        );
    }
    if tail.len() == 2 && tail[1] == "cancel" && method == Method::Post {
        query(env,meter,"UPDATE vector_store_file_batches SET status='cancelled' WHERE id=?1 AND account_id=?2 AND vector_store_id=?3 AND status='in_progress'",&[json!(batch),json!(owner),json!(store)]).await?;
        query(env,meter,"UPDATE vector_store_files SET status='cancelled',lease_id=NULL,lease_until=0 WHERE batch_id=?1 AND account_id=?2 AND vector_store_id=?3 AND status='in_progress'",&[json!(batch),json!(owner),json!(store)]).await?;
    } else if tail.len() != 1 || method != Method::Get {
        return method_not_allowed();
    }
    batch_response(env, owner, store, batch, meter).await
}

async fn batch_response(
    env: &RouteContext<'_>,
    owner: &str,
    store: &str,
    batch: &str,
    meter: &ResourceMeter<'_>,
) -> Result<Response> {
    let rows=query(env,meter,"SELECT b.id,b.created_at,b.status,COALESCE(SUM(vf.status='in_progress'),0) AS in_progress,COALESCE(SUM(vf.status='completed'),0) AS completed,COALESCE(SUM(vf.status='failed'),0) AS failed,COALESCE(SUM(vf.status='cancelled'),0) AS cancelled,COUNT(vf.file_id) AS total FROM vector_store_file_batches b LEFT JOIN vector_store_files vf ON vf.batch_id=b.id AND vf.account_id=b.account_id AND vf.vector_store_id=b.vector_store_id WHERE b.id=?1 AND b.account_id=?2 AND b.vector_store_id=?3 GROUP BY b.id",&[json!(batch),json!(owner),json!(store)]).await?.results::<Value>()?;
    let Some(row) = rows.first() else {
        return missing();
    };
    if row["in_progress"].as_u64().unwrap_or(0) > 0 {
        wake(env, owner, store, None);
    }
    let status = if row["status"] == "cancelled" {
        "cancelled"
    } else if row["in_progress"].as_u64().unwrap_or(0) > 0 {
        "in_progress"
    } else {
        "completed"
    };
    Response::from_json(
        &json!({"id":batch,"object":"vector_store.files_batch","vector_store_id":store,"created_at":row["created_at"].as_u64().unwrap_or(0)/1000,"status":status,"file_counts":{"in_progress":row["in_progress"],"completed":row["completed"],"failed":row["failed"],"cancelled":row["cancelled"],"total":row["total"]}}),
    )
}

fn wake(context: &RouteContext<'_>, owner: &str, store: &str, file: Option<&str>) {
    let env = (**context).clone();
    let owner = owner.to_string();
    let store = store.to_string();
    let file = file.map(str::to_owned);
    context.defer(async move {
        if crate::file_index_scheduler::kickoff(env, owner, store, file)
            .await
            .is_err()
        {
            util::emit("file_search.kickoff_deferred", json!({}));
        }
    });
}
