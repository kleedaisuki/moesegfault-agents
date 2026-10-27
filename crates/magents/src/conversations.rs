use anyhow::Result;
use clap::{Subcommand, ValueEnum};
use std::path::PathBuf;

use crate::{client::Api, read_json, segment, space::Page};

#[derive(Subcommand)]
pub enum Conversations {
    /// Read JSON {"metadata":{},"items":[]} from FILE or '-'; either field may be omitted.
    Create {
        file: PathBuf,
        /// Reuse only to recover this exact create, not to create a different conversation.
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    Show {
        id: String,
    },
    /// Replace metadata from JSON {"metadata":{...}} in FILE or '-'.
    Update {
        id: String,
        file: PathBuf,
    },
    /// Delete the conversation object; retained items require the separate purge operation.
    Delete {
        id: String,
    },
    /// Permanently purge retained items after delete and completion of active responses.
    Purge {
        id: String,
    },
    /// Append, page, inspect, or delete original conversation items.
    Items {
        id: String,
        #[command(subcommand)]
        command: Items,
    },
}

#[derive(Subcommand)]
pub enum Items {
    List {
        #[command(flatten)]
        page: Page,
        #[arg(long, value_enum, default_value = "desc")]
        order: Order,
    },
    Show {
        item: String,
    },
    /// Append JSON {"items":[...]} from FILE or '-'; never automatically retries.
    Create {
        file: PathBuf,
    },
    Delete {
        item: String,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Order {
    Asc,
    Desc,
}

pub async fn run(api: &Api, command: Conversations) -> Result<()> {
    match command {
        Conversations::Create {
            file,
            idempotency_key,
        } => {
            api.response_operation(
                "/v1/conversations",
                read_json(&file)?,
                idempotency_key.as_deref(),
            )
            .await
        }
        Conversations::Show { id } => api.response_object("GET", &path(&id)?, None).await,
        Conversations::Update { id, file } => {
            api.response_operation(&path(&id)?, read_json(&file)?, None)
                .await
        }
        Conversations::Delete { id } => api.response_object("DELETE", &path(&id)?, None).await,
        Conversations::Purge { id } => {
            api.response_object("POST", &format!("{}/purge", path(&id)?), None)
                .await
        }
        Conversations::Items { id, command } => {
            items(api, &format!("{}/items", path(&id)?), command).await
        }
    }
}

async fn items(api: &Api, path: &str, command: Items) -> Result<()> {
    match command {
        Items::List { page, order } => {
            let order = match order {
                Order::Asc => "asc",
                Order::Desc => "desc",
            };
            api.response_object(
                "GET",
                &format!("{path}?{}&order={order}", page.query()?),
                None,
            )
            .await
        }
        Items::Show { item } => {
            api.response_object("GET", &format!("{path}/{}", segment(&item)?), None)
                .await
        }
        Items::Create { file } => api.response_operation(path, read_json(&file)?, None).await,
        Items::Delete { item } => {
            api.response_object("DELETE", &format!("{path}/{}", segment(&item)?), None)
                .await
        }
    }
}

fn path(id: &str) -> Result<String> {
    Ok(format!("/v1/conversations/{}", segment(id)?))
}
