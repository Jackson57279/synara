//! `synara`: the native desktop client.
//!
//! ```text
//! synara --url http://127.0.0.1:3773 --token …   # an existing server
//! synara --spawn ~/synara                         # supervise a local server
//! synara --instabox synara-native                 # a server in a sandbox
//! ```

// Release builds on Windows are GUI apps: no console window opens with them.
// Debug builds keep the console so logs and `--help` stay visible.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod backend;
mod keymap;
mod model;
mod pending;
mod worklog;
mod workspace;

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use clap::Parser;
use gpui_kit::*;
use url::Url;

use crate::backend::{Backend, Target};
use crate::workspace::Workspace;

#[derive(Parser)]
#[command(version, about = "Synara native client")]
struct Cli {
    /// Server HTTP origin.
    #[arg(long, env = "SYNARA_URL", default_value = "http://127.0.0.1:3773")]
    url: Url,
    /// Loopback auth token (`SYNARA_AUTH_TOKEN` on the server).
    #[arg(long, env = "SYNARA_AUTH_TOKEN")]
    token: Option<String>,
    /// Start `apps/server` from this checkout and supervise it. Overrides
    /// `--url`/`--token`.
    #[arg(long, value_name = "REPO", conflicts_with = "instabox")]
    spawn: Option<PathBuf>,
    /// `SYNARA_HOME` for `--spawn`. Defaults to `~/.synara-native`, never the
    /// desktop apps' `~/.synara` or `~/.synara-beta`.
    #[arg(long, value_name = "DIR", requires = "spawn")]
    home: Option<PathBuf>,
    /// Bring up the server in this instabox sandbox and forward it.
    #[arg(long, value_name = "SANDBOX")]
    instabox: Option<String>,
}

impl Cli {
    fn target(self) -> Result<Target> {
        if let Some(sandbox) = self.instabox {
            return Ok(Target::Instabox {
                sandbox,
                token: self.token,
            });
        }
        if let Some(repo) = self.spawn {
            let home = match self.home {
                Some(home) => home,
                None => std::env::home_dir()
                    .context("could not find the home directory")?
                    .join(".synara-native"),
            };
            return Ok(Target::Spawn { repo, home });
        }
        Ok(Target::Url {
            url: self.url,
            token: self.token,
        })
    }
}

actions!(synara, [Quit]);

/// macOS shows an app menu even when it is empty; give it the standard Quit
/// item so cmd-q works. Other platforms quit when the last window closes.
fn install_app_menu(cx: &mut App) {
    cx.on_action(|_: &Quit, cx| cx.quit());
    if cfg!(target_os = "macos") {
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.set_menus([Menu::new("Synara").items([MenuItem::action("Quit Synara", Quit)])]);
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let target = Cli::parse().target()?;
    let (backend, events) = Backend::start(target).context("start the backend runtime")?;

    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        install_app_menu(cx);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let options = WindowOptions {
            titlebar: Some(TitlebarOptions {
                title: Some("Synara".into()),
                ..TitlebarOptions::default()
            }),
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1200.), px(800.)),
                cx,
            ))),
            ..WindowOptions::default()
        };
        gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| Workspace::new(backend, events, window, cx))
        })
        .expect("open the main window");
    });
    Ok(())
}
