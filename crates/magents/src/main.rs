mod auth;
mod client;
mod config;
mod downloads;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use client::Api;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

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
    Login {
        /// Print the authorization URL instead of opening the system browser.
        #[arg(long)]
        no_browser: bool,
    },
    Logout,
    Status,
}
#[derive(Subcommand)]
enum Agents {
    List,
    /// Write a complete versioned TOML template without login or network access.
    Init {
        #[arg(default_value = "agent.toml")]
        file: PathBuf,
    },
    /// Validate agent TOML offline. Use '-' to read from stdin.
    Validate {
        file: PathBuf,
    },
    /// Upload TOML as a callable agent in your account; existing session pins never change.
    Publish {
        file: PathBuf,
        /// Replace this owned agent instead of creating a new one.
        #[arg(long)]
        agent: Option<String>,
    },
    /// Download portable editable TOML, without account metadata or API keys.
    Download {
        id: String,
        /// New output file; '-' explicitly writes TOML to stdout. Never overwrites.
        #[arg(short, long, default_value = "agent.toml")]
        output: PathBuf,
    },
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
    /// Export the original conversation journal, including tool results and pending steering.
    Export {
        id: String,
        #[arg(long, value_enum, default_value = "json")]
        format: downloads::ExportFormat,
        /// New output file; '-' explicitly writes to stdout. Never overwrites.
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Download the committed workspace of a closed session; never starts a container.
    Workspace {
        #[command(subcommand)]
        command: Workspace,
    },
}

#[derive(Subcommand)]
enum Workspace {
    /// Download a complete integrity-checked tar.zst snapshot, without unpacking it.
    Download {
        id: String,
        /// New archive file; '-' explicitly writes binary stdout. Never overwrites.
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Download one regular file from the snapshot; links and unsafe paths are rejected.
    File {
        id: String,
        path: String,
        #[arg(short, long)]
        output: PathBuf,
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
    /// Add an OpenAI-compatible or Anthropic endpoint. The key is read with a hidden prompt.
    Add {
        #[arg(value_enum)]
        protocol: ByokProtocol,
        #[arg(long)]
        base_url: String,
        #[arg(long)]
        name: Option<String>,
    },
    /// Rotate an owned credential key. Legacy openrouter/deepseek aliases remain supported.
    Set {
        id: String,
        /// Existing credential routes are immutable; use add to select a different endpoint.
        #[arg(long, value_enum, requires = "base_url")]
        protocol: Option<ByokProtocol>,
        #[arg(long, requires = "protocol")]
        base_url: Option<String>,
        #[arg(long)]
        name: Option<String>,
    },
    Delete {
        id: String,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum ByokProtocol {
    #[value(alias = "openai-compatible")]
    Openai,
    Anthropic,
}

impl ByokProtocol {
    fn as_str(self) -> &'static str {
        match self {
            Self::Openai => "openai",
            Self::Anthropic => "anthropic",
        }
    }
}

fn read_json(path: &Path) -> Result<Value> {
    let source = config::read_source(path)?;
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

async fn agents(api: &Api, command: Agents) -> Result<()> {
    match command {
        Agents::Init { file } => {
            downloads::write_bytes(&file, config::AgentFile::template()?.toml()?.as_bytes())
        }
        Agents::Validate { file } => {
            config::AgentFile::read(&file)?;
            println!("{{\"valid\":true,\"schema_version\":1}}");
            Ok(())
        }
        Agents::Publish { file, agent } => {
            let path = match &agent {
                Some(id) => format!("/v1/agents/{}", segment(id)?),
                None => "/v1/agents".into(),
            };
            let body = config::AgentFile::read(&file)?.api_body()?;
            api.print(
                if agent.is_some() { "PUT" } else { "POST" },
                &path,
                Some(body),
            )
            .await
        }
        Agents::Download { id, output } => {
            use std::io::Write;
            let mut destination = downloads::Output::new(&output, false)?;
            let value = api
                .json("GET", &format!("/v1/agents/{}", segment(&id)?), None)
                .await?;
            let config = config::AgentFile::from_api(value)?;
            destination.writer().write_all(config.toml()?.as_bytes())?;
            destination.finish()
        }
        command => {
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
                _ => unreachable!(),
            };
            api.print(method, &path, body).await
        }
    }
}

fn credential_payload(
    key: String,
    protocol: Option<ByokProtocol>,
    base_url: Option<String>,
    name: Option<String>,
) -> Value {
    let mut payload = json!({"api_key":key});
    if let Some(protocol) = protocol {
        payload["protocol"] = protocol.as_str().into();
    }
    if let Some(base_url) = base_url {
        payload["base_url"] = base_url.into();
    }
    if let Some(name) = name {
        payload["name"] = name.into();
    }
    payload
}

async fn byok(api: &Api, command: Byok) -> Result<()> {
    match command {
        Byok::List => api.print("GET", "/v1/provider-keys", None).await,
        Byok::Add {
            protocol,
            base_url,
            name,
        } => {
            let key = rpassword::prompt_password("API key: ")?;
            let body = credential_payload(key, Some(protocol), Some(base_url), name);
            api.print("POST", "/v1/provider-keys", Some(body)).await
        }
        Byok::Set {
            id,
            protocol,
            base_url,
            name,
        } => {
            let path = format!("/v1/provider-keys/{}", segment(&id)?);
            let key = rpassword::prompt_password("API key: ")?;
            let body = credential_payload(key, protocol, base_url, name);
            api.print("PUT", &path, Some(body)).await
        }
        Byok::Delete { id } => {
            api.print(
                "DELETE",
                &format!("/v1/provider-keys/{}", segment(&id)?),
                None,
            )
            .await
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let api = Api::new(&cli.api_url)?;
    match cli.command {
        Command::Auth { command } => match command {
            Auth::Login { no_browser } => auth::login(api.base(), no_browser).await?,
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
        Command::Agents { command } => agents(&api, command).await?,
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
            Sessions::Export { id, format, output } => {
                downloads::transcript(&api, segment(&id)?, format, &output).await?;
            }
            Sessions::Workspace { command } => match command {
                Workspace::Download { id, output } => {
                    downloads::workspace(&api, segment(&id)?, &output).await?;
                }
                Workspace::File { id, path, output } => {
                    downloads::workspace_file(&api, segment(&id)?, &path, &output).await?;
                }
            },
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
        Command::Byok { command } => byok(&api, command).await?,
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

    #[test]
    fn byok_protocols_and_hidden_key_contract_are_explicit() {
        for protocol in ["openai", "openai-compatible", "anthropic"] {
            assert!(
                Cli::try_parse_from([
                    "magents",
                    "byok",
                    "add",
                    protocol,
                    "--base-url",
                    "https://provider.test/v1"
                ])
                .is_ok()
            );
        }
        assert!(
            Cli::try_parse_from([
                "magents",
                "byok",
                "add",
                "deepseek",
                "--base-url",
                "https://provider.test/v1"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "magents",
                "byok",
                "add",
                "openai",
                "--base-url",
                "https://provider.test/v1",
                "--api-key",
                "never-an-argument"
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["magents", "byok", "set", "deepseek"]).is_ok());
        assert!(
            Cli::try_parse_from(["magents", "byok", "set", "key-id", "--protocol", "openai"])
                .is_err()
        );
        assert_eq!(
            credential_payload(
                "secret-fixture".into(),
                Some(ByokProtocol::Anthropic),
                Some("https://provider.test".into()),
                Some("research".into())
            ),
            json!({"api_key":"secret-fixture","protocol":"anthropic","base_url":"https://provider.test","name":"research"})
        );
        assert_eq!(
            credential_payload("rotated-fixture".into(), None, None, None),
            json!({"api_key":"rotated-fixture"})
        );
    }
}
