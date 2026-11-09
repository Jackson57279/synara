//! `synara-probe session`: the same lifecycle the app runs, minus the window.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use synara_protocol::commands::{ClientCommand, InteractionMode, RuntimeMode};
use synara_protocol::ids::{ProjectId, ThreadId};
use synara_protocol::orchestration::{MessageRole, ModelSelection};
use synara_session::{ConnectionState, Session, SessionConfig, SessionEvent, SessionState};
use synara_sync::ThreadStatus;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::Cli;

/// Waits until `done` holds on the session state, draining events meanwhile.
async fn until(
    session: &Session,
    events: &mut UnboundedReceiver<SessionEvent>,
    step: Duration,
    what: &str,
    done: impl Fn(&SessionState) -> bool,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + step;
    loop {
        if done(&session.state()) {
            return Ok(());
        }
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Err(_) => bail!(
                "timed out waiting for {what} (connection: {:?})",
                session.state().connection
            ),
            Ok(None) => bail!("session ended while waiting for {what}"),
            Ok(Some(SessionEvent::ThreadFailed { thread_id, error })) => {
                bail!("thread {thread_id} failed while waiting for {what}: {error}")
            }
            Ok(Some(_)) => {}
        }
    }
}

fn connected_instance(state: &SessionState) -> Option<String> {
    match &state.connection {
        ConnectionState::Connected {
            server_instance_id, ..
        } => Some(server_instance_id.clone()),
        _ => None,
    }
}

pub async fn run(
    cli: &Cli,
    step: Duration,
    workspace: &str,
    provider: &str,
    model: &str,
) -> Result<()> {
    let mut config = SessionConfig::new(cli.url.clone());
    config.token = cli.token.clone();
    config.bearer = cli.bearer.clone();
    let (session, mut events) = Session::start(config);

    until(&session, &mut events, step, "connect", |s| {
        connected_instance(s).is_some()
    })
    .await?;
    until(&session, &mut events, step, "shell snapshot", |s| {
        s.shell.is_loaded()
    })
    .await?;
    let instance = connected_instance(&session.state()).unwrap_or_default();
    println!("ok   session connected to instance {instance}");

    let mut project = ClientCommand::project_create("Native session probe", workspace).to_value();
    project["createWorkspaceRootIfMissing"] = serde_json::json!(true);
    let project_id = ProjectId::from(project["projectId"].as_str().unwrap_or_default());
    let client_command: ClientCommand = match ClientCommand::project_create("x", workspace) {
        ClientCommand::ProjectCreate {
            command_id,
            created_at,
            ..
        } => ClientCommand::ProjectCreate {
            command_id,
            project_id: project_id.clone(),
            title: "Native session probe".into(),
            workspace_root: workspace.into(),
            create_workspace_root_if_missing: true,
            default_model_selection: None,
            created_at,
        },
        _ => unreachable!(),
    };
    session
        .dispatch(&client_command)
        .await
        .context("project.create")?;

    let thread = ClientCommand::thread_create(
        project_id,
        "Session probe thread",
        ModelSelection::new(provider, model),
    );
    let ClientCommand::ThreadCreate { thread_id, .. } = &thread else {
        unreachable!()
    };
    let thread_id: ThreadId = thread_id.clone();
    session.dispatch(&thread).await.context("thread.create")?;
    until(&session, &mut events, step, "thread in shell", |s| {
        s.shell.thread(&thread_id).is_some()
    })
    .await?;
    println!("ok   project and thread created through the session");

    session.open_thread(&thread_id);
    until(&session, &mut events, step, "thread snapshot", |s| {
        s.threads
            .get(&thread_id)
            .is_some_and(|t| t.status() == ThreadStatus::Ready)
    })
    .await?;
    println!("ok   thread subscribed");

    let sequence = session
        .send_message(
            &thread_id,
            "Say hi from the native probe.".into(),
            RuntimeMode::FullAccess,
            InteractionMode::Default,
        )
        .await
        .context("send_message")?;
    println!("ok   thread.turn.start accepted at sequence {sequence}");
    until(&session, &mut events, step, "user message echo", |s| {
        s.threads.get(&thread_id).is_some_and(|t| {
            t.messages()
                .iter()
                .any(|m| m.role == MessageRole::User && !t.is_pending(&m.id))
        })
    })
    .await?;
    println!("ok   user message echoed and reconciled");

    let cursor_before = session
        .state()
        .threads
        .get(&thread_id)
        .and_then(|t| t.cursor());
    session.reconnect();
    until(&session, &mut events, step, "disconnect", |s| {
        connected_instance(s).is_none()
    })
    .await?;
    until(&session, &mut events, step, "reconnect", |s| {
        connected_instance(s).is_some()
    })
    .await?;
    until(&session, &mut events, step, "thread resume", |s| {
        s.threads
            .get(&thread_id)
            .is_some_and(|t| t.status() == ThreadStatus::Ready && t.cursor() >= cursor_before)
    })
    .await?;
    let state = session.state();
    let store = state
        .threads
        .get(&thread_id)
        .context("thread store vanished")?;
    println!(
        "ok   reconnected; thread resumed from cursor {:?} to {:?} with {} messages",
        cursor_before,
        store.cursor(),
        store.messages().len()
    );
    // Give a provider a moment to report whatever it does without credentials.
    let activities = store
        .thread()
        .map(|t| t.activities.len())
        .unwrap_or_default();
    println!(
        "info thread has {activities} activities; working: {}",
        store.is_working()
    );
    drop(state);
    println!("all probe steps passed");
    Ok(())
}
