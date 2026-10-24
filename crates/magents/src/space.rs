use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use std::path::PathBuf;

use crate::{client::Api, downloads, segment};

pub const MAX_UPLOAD_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Subcommand)]
pub enum Space {
    /// Show your Space and its storage usage.
    Show,
    /// Rename your Space without moving or deleting its files.
    Rename { name: String },
    /// Upload and manage your files.
    Files {
        #[command(subcommand)]
        command: Files,
    },
    /// Discover and download immutable workspace versions across your Space.
    Versions {
        #[command(subcommand)]
        command: Versions,
    },
}

#[derive(Args)]
pub struct Page {
    /// Maximum results in this page. Follow last_id when has_more is true.
    #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u16).range(1..=100))]
    limit: u16,
    /// Continue after the previous page's last_id.
    #[arg(long)]
    after: Option<String>,
}

impl Page {
    pub fn query(&self) -> Result<String> {
        let mut query = format!("limit={}", self.limit);
        if let Some(after) = &self.after {
            query.push_str("&after=");
            query.push_str(segment(after)?);
        }
        Ok(query)
    }
}

#[derive(Subcommand)]
pub enum Files {
    List {
        #[command(flatten)]
        page: Page,
    },
    /// Upload one regular file, streamed from disk; never automatically retries.
    Upload {
        file: PathBuf,
        /// Use this stored filename instead of the local basename.
        #[arg(long)]
        name: Option<String>,
        /// Media type of the uploaded bytes.
        #[arg(long, default_value = "application/octet-stream")]
        content_type: String,
    },
    Show {
        id: String,
    },
    Rename {
        id: String,
        name: String,
    },
    /// Download a file with length and checksum verification. Never overwrites.
    Download {
        id: String,
        /// New output file; '-' explicitly selects binary stdout.
        #[arg(short, long)]
        output: PathBuf,
    },
    Delete {
        id: String,
    },
}

#[derive(Subcommand)]
pub enum Versions {
    List {
        #[command(flatten)]
        page: Page,
    },
    Show {
        id: String,
    },
    /// Download a complete integrity-checked archive. Never overwrites or extracts.
    Download {
        id: String,
        #[arg(short, long)]
        output: PathBuf,
    },
}

pub async fn run(api: &Api, command: Space) -> Result<()> {
    match command {
        Space::Show => api.print("GET", "/v1/space", None).await,
        Space::Rename { name } => {
            api.print("PATCH", "/v1/space", Some(serde_json::json!({"name":name})))
                .await
        }
        Space::Files { command } => files(api, command).await,
        Space::Versions { command } => versions(api, command).await,
    }
}

async fn files(api: &Api, command: Files) -> Result<()> {
    match command {
        Files::List { page } => {
            api.print("GET", &format!("/v1/files?{}", page.query()?), None)
                .await
        }
        Files::Upload {
            file,
            name,
            content_type,
        } => {
            let name = upload_name(&file, name)?;
            let query = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("filename", &name)
                .append_pair("purpose", "user_data")
                .finish();
            let record = api
                .upload_file(&format!("/v1/space/files?{query}"), &file, &content_type)
                .await?;
            println!("{}", serde_json::to_string_pretty(&record)?);
            Ok(())
        }
        Files::Show { id } => api.print("GET", &file_path(&id)?, None).await,
        Files::Rename { id, name } => {
            validate_filename(&name)?;
            api.print(
                "PATCH",
                &file_path(&id)?,
                Some(serde_json::json!({"filename":name})),
            )
            .await
        }
        Files::Download { id, output } => {
            let mut destination = downloads::Output::new(&output, true)?;
            api.download_file_to(
                &format!("{}/content", file_path(&id)?),
                destination.writer(),
                MAX_UPLOAD_BYTES,
            )
            .await?;
            destination.finish()
        }
        Files::Delete { id } => api.print("DELETE", &file_path(&id)?, None).await,
    }
}

async fn versions(api: &Api, command: Versions) -> Result<()> {
    let base = "/v1/space/workspace-versions";
    match command {
        Versions::List { page } => {
            api.print("GET", &format!("{base}?{}", page.query()?), None)
                .await
        }
        Versions::Show { id } => {
            api.print("GET", &format!("{base}/{}", segment(&id)?), None)
                .await
        }
        Versions::Download { id, output } => {
            let mut destination = downloads::Output::new(&output, true)?;
            api.download_to(
                &format!("{base}/{}/content", segment(&id)?),
                destination.writer(),
                downloads::MAX_ARCHIVE_BYTES,
                true,
            )
            .await?;
            destination.finish()
        }
    }
}

fn file_path(id: &str) -> Result<String> {
    Ok(format!("/v1/files/{}", segment(id)?))
}

fn upload_name(path: &std::path::Path, name: Option<String>) -> Result<String> {
    let name = match name {
        Some(name) => name,
        None => path
            .file_name()
            .and_then(|name| name.to_str())
            .context("Upload filename must be UTF-8; use --name to select a stored name")?
            .to_owned(),
    };
    validate_filename(&name)?;
    Ok(name)
}

fn validate_filename(name: &str) -> Result<()> {
    if name.trim().is_empty()
        || name.len() > 255
        || name.contains(['/', '\\', '\0', '\r', '\n'])
        || matches!(name, "." | "..")
    {
        bail!(
            "Stored filename must contain 1 to 255 UTF-8 bytes without path separators or newlines"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filenames_are_names_not_paths_or_headers() {
        for name in ["", " ", ".", "..", "a/b", "a\\b", "a\r\nb", "a\0b"] {
            assert!(validate_filename(name).is_err(), "{name:?}");
        }
        assert!(validate_filename("研究 notes.txt").is_ok());
        assert!(validate_filename(&"x".repeat(256)).is_err());
        assert_eq!(
            upload_name(std::path::Path::new("a/b.txt"), None).unwrap(),
            "b.txt"
        );
    }
}
