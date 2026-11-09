//! `synara-probe`: drives a real server through the same crates the GPUI app
//! uses, without a window. This is the verification path for hosts the app
//! cannot reach yet (Stable rejects remote connections).

use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use synara_protocol::commands::{ClientCommand, DispatchResult};
use synara_protocol::ids::{ProjectId, ThreadId};
use synara_protocol::negotiate::ClientOffer;
use synara_protocol::orchestration::{
    ModelSelection, ShellStreamItem, SubscribeThreadInput, ThreadStreamItem,
};
use synara_protocol::tags;
use synara_rpc::negotiate::negotiate;
use synara_rpc::{ClientConfig, RpcClient, RpcStream};
use synara_sync::{ShellStore, ThreadStatus, ThreadStore};
use url::Url;

mod session;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Server HTTP origin.
    #[arg(long, env = "SYNARA_URL", default_value = "http://127.0.0.1:3773")]
    url: Url,
    /// Loopback auth token (`SYNARA_AUTH_TOKEN` on the server), sent as `?token=`.
    #[arg(long, env = "SYNARA_AUTH_TOKEN")]
    token: Option<String>,
    /// Bearer session for remote hosts.
    #[arg(long, env = "SYNARA_BEARER")]
    bearer: Option<String>,
    /// Per-step timeout in seconds.
    #[arg(long, default_value_t = 20)]
    timeout: u64,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Negotiate, connect, and load the shell snapshot.
    Check,
    /// `check`, then create a project and thread and load the thread snapshot.
    Smoke {
        /// Workspace root for the throwaway project (created if missing).
        #[arg(long, default_value = "/tmp/synara-probe")]
        workspace: String,
        #[arg(long, default_value = "codex")]
        provider: String,
        #[arg(long, default_value = "gpt-5.5")]
        model: String,
    },
    /// Drive `synara-session`: create, open, send, force a reconnect, resume.
    Session {
        #[arg(long, default_value = "/tmp/synara-probe")]
        workspace: String,
        #[arg(long, default_value = "codex")]
        provider: String,
        #[arg(long, default_value = "gpt-5.5")]
        model: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let step = Duration::from_secs(cli.timeout);
    if let Command::Session {
        workspace,
        provider,
        model,
    } = &cli.command
    {
        return session::run(&cli, step, workspace, provider, model).await;
    }

    let client = connect(&cli, step).await?;
    let mut shell = ShellStore::new();
    let mut shell_stream = client
        .stream(tags::ORCHESTRATION_SUBSCRIBE_SHELL, json!({}))
        .await?;
    while !shell.is_loaded() {
        let item: ShellStreamItem = next_item(&mut shell_stream, step, "shell snapshot").await?;
        shell.apply(item);
    }
    println!(
        "ok   shell snapshot at sequence {}: {} spaces, {} projects, {} threads",
        shell.cursor().unwrap_or_default(),
        shell.spaces().len(),
        shell.projects().count(),
        shell.threads().count()
    );

    if let Command::Smoke {
        workspace,
        provider,
        model,
    } = &cli.command
    {
        smoke(
            &client,
            &mut shell,
            &mut shell_stream,
            step,
            workspace,
            provider,
            model,
        )
        .await?;
    }
    client.close();
    println!("all probe steps passed");
    Ok(())
}

async fn connect(cli: &Cli, step: Duration) -> Result<RpcClient> {
    let offer = ClientOffer::native(env!("CARGO_PKG_VERSION"));
    let http = reqwest::Client::builder().timeout(step).build()?;
    let negotiated = negotiate(&http, &cli.url, &offer)
        .await
        .context("negotiate")?;
    println!(
        "ok   negotiate: server {} revision {} instance {} ({} capabilities)",
        negotiated.server_build,
        negotiated.negotiated_revision,
        negotiated.server_instance_id,
        negotiated.capabilities.len()
    );
    let missing: Vec<_> = offer
        .required_capabilities
        .iter()
        .filter(|c| !negotiated.has_capability(c))
        .collect();
    if !missing.is_empty() {
        bail!("server lacks required capabilities: {missing:?}");
    }

    let url = offer.connect_url(&cli.url, &negotiated, cli.token.as_deref());
    let config = ClientConfig {
        bearer: cli.bearer.clone(),
        ..ClientConfig::default()
    };
    let client = tokio::time::timeout(step, RpcClient::connect(&url, config))
        .await
        .context("connect timed out")?
        .context("connect")?;
    println!("ok   websocket connected");
    Ok(client)
}

async fn next_item<T: serde::de::DeserializeOwned>(
    stream: &mut RpcStream,
    step: Duration,
    what: &str,
) -> Result<T> {
    match tokio::time::timeout(step, stream.next_as::<T>()).await {
        Err(_) => bail!("timed out waiting for {what}"),
        Ok(None) => bail!("stream ended before {what}"),
        Ok(Some(item)) => item.with_context(|| format!("reading {what}")),
    }
}

async fn dispatch(client: &RpcClient, command: Value, step: Duration) -> Result<DispatchResult> {
    let kind = command["type"].as_str().unwrap_or("?").to_owned();
    let result = tokio::time::timeout(
        step,
        client.call(tags::ORCHESTRATION_DISPATCH_COMMAND, &command),
    )
    .await
    .with_context(|| format!("{kind} timed out"))?
    .with_context(|| format!("dispatch {kind}"))?;
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
async fn smoke(
    client: &RpcClient,
    shell: &mut ShellStore,
    shell_stream: &mut RpcStream,
    step: Duration,
    workspace: &str,
    provider: &str,
    model: &str,
) -> Result<()> {
    let mut project = ClientCommand::project_create("Native probe", workspace).to_value();
    project["createWorkspaceRootIfMissing"] = json!(true);
    let project_id = ProjectId::from(project["projectId"].as_str().unwrap_or_default());
    let accepted = dispatch(client, project, step).await?;
    println!(
        "ok   project.create accepted at sequence {}",
        accepted.sequence
    );

    let thread = ClientCommand::thread_create(
        project_id.clone(),
        "Native probe thread",
        ModelSelection::new(provider, model),
    )
    .to_value();
    let thread_id = ThreadId::from(thread["threadId"].as_str().unwrap_or_default());
    let accepted = dispatch(client, thread, step).await?;
    println!(
        "ok   thread.create accepted at sequence {}",
        accepted.sequence
    );

    while shell.thread(&thread_id).is_none() {
        let item: ShellStreamItem = next_item(shell_stream, step, "thread-upserted").await?;
        shell.apply(item);
    }
    let rows = shell.sidebar(None);
    let row = rows
        .iter()
        .find(|r| r.project.id == project_id)
        .context("project missing from the sidebar")?;
    if !row.threads.iter().any(|t| t.id == thread_id) {
        bail!("thread missing from its project's sidebar row");
    }
    println!("ok   shell stream shows the new project and thread");

    let mut store = ThreadStore::new(thread_id.clone());
    let mut thread_stream = client
        .stream(
            tags::ORCHESTRATION_SUBSCRIBE_THREAD,
            serde_json::to_value(SubscribeThreadInput::new(thread_id, None))?,
        )
        .await?;
    while store.status() == ThreadStatus::Loading {
        let item: ThreadStreamItem = next_item(&mut thread_stream, step, "thread snapshot").await?;
        store.apply(item);
    }
    println!(
        "ok   thread snapshot at sequence {}: \"{}\", {} messages",
        store.cursor().unwrap_or_default(),
        store.thread().map(|t| t.title.as_str()).unwrap_or_default(),
        store.messages().len()
    );
    Ok(())
}
