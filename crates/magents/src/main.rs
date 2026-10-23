mod auth;
mod client;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use client::Api;
use serde_json::{Value, json};
use std::{io::Read, path::PathBuf};

/// Your agents, from a terminal. Add --help to any command to explore further.
#[derive(Parser)]
#[command(version, about, arg_required_else_help = true)]
struct Cli {
    #[arg(
        long,
        env = "MAGENTS_API_URL",
        default_value = "https://agents.moesegfault.dev",
        global = true
    )]
    api_url: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Send a prompt and stream the agent's response.
    Run {
        agent: String,
        prompt: String,
        /// Continue an existing session (reported on stderr).
        #[arg(long)]
        session: Option<String>,
    },
    /// Manage moeSegFault account authentication.
    Auth {
        #[command(subcommand)]
        command: Auth,
    },
    /// Discover and manage agent configurations.
    Agents {
        #[command(subcommand)]
        command: Agents,
    },
    /// Manage personalized skills pinned into new sessions.
    Skills {
        #[command(subcommand)]
        command: Skills,
    },
    /// Start, inspect, steer, or close conversations.
    Sessions {
        #[command(subcommand)]
        command: Sessions,
    },
    /// List upstream models and their capabilities.
    Models,
    /// Show metered resource and model usage.
    Usage,
    /// Show account plan and limits.
    Account,
    /// Manage API access keys.
    Keys {
        #[command(subcommand)]
        command: Keys,
    },
    /// Manage your upstream provider credentials.
    Byok {
        #[command(subcommand)]
        command: Byok,
    },
    /// Preview or explicitly upload a minimal diagnostics report.
    Diagnostics {
        #[arg(long, requires = "consent")]
        upload: bool,
        #[arg(long, requires = "upload")]
        consent: bool,
    },
}

#[derive(Subcommand)]
enum Auth {
    Login,
    Logout,
    Status,
}
#[derive(Subcommand)]
enum Agents {
    List,
    /// Read the API agent configuration JSON from a file, or '-' for stdin.
    Create {
        file: PathBuf,
    },
    Show {
        id: String,
    },
    Update {
        id: String,
        file: PathBuf,
    },
    Delete {
        id: String,
    },
}

#[derive(Subcommand)]
enum Skills {
    List,
    Show {
        name: String,
    },
    /// Read JSON {"name":"...","instructions":"..."} from a file or '-' for stdin.
    Create {
        file: PathBuf,
    },
    /// Read JSON {"instructions":"..."} from a file or '-' for stdin.
    Update {
        name: String,
        file: PathBuf,
    },
    Delete {
        name: String,
    },
}
#[derive(Subcommand)]
enum Sessions {
    List,
    Resume {
        id: String,
    },
    Start {
        agent: String,
    },
    Show {
        id: String,
    },
    /// Append information while an agent is running.
    Steer {
        id: String,
        content: String,
    },
    Close {
        id: String,
    },
}
#[derive(Subcommand)]
enum Keys {
    List,
    Create { name: String },
    Delete { id: String },
}
#[derive(Subcommand)]
enum Byok {
    List,
    /// Read a provider key using a hidden terminal prompt, never a command argument.
    Set {
        provider: String,
    },
    Delete {
        provider: String,
    },
}

fn read_json(path: &PathBuf) -> Result<Value> {
    let mut source = String::new();
    if path.as_os_str() == "-" {
        std::io::stdin().read_to_string(&mut source)?;
    } else {
        source = std::fs::read_to_string(path).context("Cannot read configuration")?;
    }
    serde_json::from_str(&source).context("Configuration must be valid JSON")
}

fn segment(value: &str) -> Result<&str> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
    {
        bail!("Identifier must contain only letters, numbers, '_' or '-'");
    }
    Ok(value)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let api = Api::new(&cli.api_url)?;
    match cli.command {
        Command::Auth { command } => match command {
            Auth::Login => auth::login(api.base()).await?,
            Auth::Logout => auth::logout(api.base())?,
            Auth::Status => auth::status(api.base())?,
        },
        Command::Run {
            agent,
            prompt,
            session,
        } => {
            api.stream(json!({"model": agent, "messages": [{"role": "user", "content": prompt}], "stream": true}), session).await?;
        }
        Command::Agents { command } => {
            let (method, path, body) = match command {
                Agents::List => ("GET", "/v1/agents".into(), None),
                Agents::Create { file } => ("POST", "/v1/agents".into(), Some(read_json(&file)?)),
                Agents::Show { id } => ("GET", format!("/v1/agents/{}", segment(&id)?), None),
                Agents::Update { id, file } => (
                    "PATCH",
                    format!("/v1/agents/{}", segment(&id)?),
                    Some(read_json(&file)?),
                ),
                Agents::Delete { id } => ("DELETE", format!("/v1/agents/{}", segment(&id)?), None),
            };
            api.print(method, &path, body).await?;
        }
        Command::Skills { command } => {
            let (method, path, body) = match command {
                Skills::List => ("GET", "/v1/skills".into(), None),
                Skills::Show { name } => ("GET", format!("/v1/skills/{}", segment(&name)?), None),
                Skills::Create { file } => ("POST", "/v1/skills".into(), Some(read_json(&file)?)),
                Skills::Update { name, file } => (
                    "PUT",
                    format!("/v1/skills/{}", segment(&name)?),
                    Some(read_json(&file)?),
                ),
                Skills::Delete { name } => {
                    ("DELETE", format!("/v1/skills/{}", segment(&name)?), None)
                }
            };
            api.print(method, &path, body).await?;
        }
        Command::Sessions { command } => match command {
            Sessions::List => api.print("GET", "/v1/sessions", None).await?,
            Sessions::Start { agent } => {
                api.print(
                    "POST",
                    &format!("/v1/agents/{}/sessions", segment(&agent)?),
                    Some(json!({})),
                )
                .await?
            }
            Sessions::Show { id } => {
                api.print("GET", &format!("/v1/sessions/{}", segment(&id)?), None)
                    .await?
            }
            Sessions::Close { id } => {
                api.print("DELETE", &format!("/v1/sessions/{}", segment(&id)?), None)
                    .await?
            }
            Sessions::Resume { id } => {
                api.print(
                    "POST",
                    &format!("/v1/sessions/{}/resume", segment(&id)?),
                    None,
                )
                .await?
            }
            Sessions::Steer { id, content } => {
                let body = json!({"content": content});
                api.print(
                    "POST",
                    &format!("/v1/sessions/{}/steer", segment(&id)?),
                    Some(body),
                )
                .await?;
            }
        },
        Command::Models => api.print("GET", "/v1/models", None).await?,
        Command::Usage => api.print("GET", "/v1/usage", None).await?,
        Command::Account => api.print("GET", "/v1/account", None).await?,
        Command::Keys { command } => match command {
            Keys::List => api.print("GET", "/v1/api-keys", None).await?,
            Keys::Create { name } => {
                api.print("POST", "/v1/api-keys", Some(json!({"name": name})))
                    .await?
            }
            Keys::Delete { id } => {
                api.print("DELETE", &format!("/v1/api-keys/{}", segment(&id)?), None)
                    .await?
            }
        },
        Command::Byok { command } => match command {
            Byok::List => api.print("GET", "/v1/provider-keys", None).await?,
            Byok::Set { provider } => {
                let key = rpassword::prompt_password("Provider key: ")?;
                api.print(
                    "PUT",
                    &format!("/v1/provider-keys/{}", segment(&provider)?),
                    Some(json!({"api_key": key})),
                )
                .await?;
            }
            Byok::Delete { provider } => {
                api.print(
                    "DELETE",
                    &format!("/v1/provider-keys/{}", segment(&provider)?),
                    None,
                )
                .await?
            }
        },
        Command::Diagnostics { upload, .. } => {
            let report = json!({"consent": true, "client": "magents", "version": env!("CARGO_PKG_VERSION"), "platform": format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)});
            println!("{}", serde_json::to_string_pretty(&report)?);
            if upload {
                api.print("POST", "/v1/diagnostics", Some(report)).await?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    #[test]
    fn command_tree_is_consistent() {
        Cli::command().debug_assert();
    }
    #[test]
    fn diagnostics_requires_explicit_consent() {
        assert!(Cli::try_parse_from(["magents", "diagnostics", "--upload"]).is_err());
        assert!(Cli::try_parse_from(["magents", "diagnostics", "--upload", "--consent"]).is_ok());
    }
    #[test]
    fn identifiers_cannot_escape_resource_paths() {
        for value in ["../account", "a/b", "a?x", "", "%2f", "α"] {
            assert!(segment(value).is_err());
        }
        assert!(segment("agent_a-123").is_ok());
    }
}
