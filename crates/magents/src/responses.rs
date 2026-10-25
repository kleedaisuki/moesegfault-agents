use anyhow::{Result, bail};
use clap::Subcommand;
use serde_json::json;
use std::path::PathBuf;

use crate::{client::Api, read_json, segment, space::Page};

#[derive(Subcommand)]
pub enum Responses {
    /// Create from Responses API JSON; use '-' for stdin. Local tools are never auto-executed.
    Create {
        file: PathBuf,
        /// Use an existing session, including its immutable model and tool pin.
        #[arg(long)]
        session: Option<String>,
        /// Stream text and report response/session identities on stderr.
        #[arg(long)]
        stream: bool,
        /// Print complete typed events as JSONL instead of text deltas.
        #[arg(long, requires = "stream")]
        events: bool,
        /// Stable operation key for explicit retry of the identical request, not a new prompt.
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    /// Start or continue a text response. Use create for files and local function tools.
    Run {
        agent: String,
        prompt: String,
        /// Use an existing session, including its immutable model and tool pin.
        #[arg(long)]
        session: Option<String>,
        /// Continue or fork a retained response.
        #[arg(long)]
        previous: Option<String>,
        /// Return a durable response immediately instead of waiting for its answer.
        #[arg(long)]
        background: bool,
        #[arg(long)]
        stream: bool,
        #[arg(long, requires = "stream")]
        events: bool,
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    /// Retrieve a saved response without replaying its input.
    Show {
        id: String,
        /// Replay saved events and follow active execution without resubmitting input.
        #[arg(long)]
        stream: bool,
        /// Resume after this event sequence number to avoid retransferring earlier events.
        #[arg(long, requires = "stream")]
        starting_after: Option<u64>,
        /// Emit complete typed events as JSONL instead of text deltas.
        #[arg(long, requires = "stream")]
        events: bool,
    },
    /// Request cancellation; inspect the returned response for its durable status.
    Cancel { id: String },
    /// Delete a saved response, not the whole session or Space.
    Delete { id: String },
    /// Page through the accepted inputs for a saved response.
    Inputs {
        id: String,
        #[command(flatten)]
        page: Page,
    },
}

pub async fn run(api: &Api, command: Responses) -> Result<()> {
    match command {
        Responses::Create {
            file,
            session,
            stream,
            events,
            idempotency_key,
        } => {
            let mut body = read_json(&file)?;
            if !body.is_object() {
                bail!("Responses request must be a JSON object");
            }
            if stream {
                body["stream"] = true.into();
            }
            let session = session
                .map(|id| segment(&id).map(str::to_owned))
                .transpose()?;
            api.create_response(body, events, idempotency_key.as_deref(), session)
                .await
        }
        Responses::Run {
            agent,
            prompt,
            session,
            previous,
            background,
            stream,
            events,
            idempotency_key,
        } => {
            let mut body =
                json!({"model":agent,"input":prompt,"stream":stream,"background":background});
            if let Some(previous) = previous {
                body["previous_response_id"] = segment(&previous)?.into();
            }
            let session = session
                .map(|id| segment(&id).map(str::to_owned))
                .transpose()?;
            api.create_response(body, events, idempotency_key.as_deref(), session)
                .await
        }
        Responses::Show {
            id,
            stream,
            starting_after,
            events,
        } => {
            let mut path = path(&id)?;
            if !stream {
                return api.response_object("GET", &path, None).await;
            }
            path.push_str("?stream=true");
            if let Some(after) = starting_after {
                path.push_str(&format!("&starting_after={after}"));
            }
            api.retrieve_response_stream(&path, events).await
        }
        Responses::Cancel { id } => {
            api.response_object("POST", &format!("{}/cancel", path(&id)?), None)
                .await
        }
        Responses::Delete { id } => api.response_object("DELETE", &path(&id)?, None).await,
        Responses::Inputs { id, page } => {
            api.response_object(
                "GET",
                &format!("{}/input_items?{}", path(&id)?, page.query()?),
                None,
            )
            .await
        }
    }
}

fn path(id: &str) -> Result<String> {
    Ok(format!("/v1/responses/{}", segment(id)?))
}
