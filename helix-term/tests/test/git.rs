//! The Git flow driven by keys in a temporary repository: edit and save,
//! `:git-status`, stage with `Alt-s`, `Alt-c` for the draft, `:w` commits.
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use helix_term::application::Application;
use helix_view::input::parse_macro;
use tokio::sync::mpsc::UnboundedSender;
use tokio_stream::wrappers::UnboundedReceiverStream;

#[cfg(windows)]
use crossterm::event::{Event, KeyEvent};
#[cfg(not(windows))]
use termina::event::{Event, KeyEvent};

use super::*;

/// Git for fixtures, without the developer's configuration.
pub fn git(root: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// A repository whose own configuration makes Helix's Git runs independent
/// of the developer's: identity, no signing and no hooks.
pub fn repository(dir: &Path) -> PathBuf {
    let root = dir.join("repo");
    fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "--quiet", "--initial-branch=topic"]);
    for (key, value) in [
        ("user.name", "t"),
        ("user.email", "t@example.com"),
        ("commit.gpgsign", "false"),
        ("core.hooksPath", dir.join("no-hooks").to_str().unwrap()),
    ] {
        git(&root, &["config", key, value]);
    }
    fs::write(root.join("a.txt"), "hello\n").unwrap();
    git(&root, &["add", "a.txt"]);
    git(&root, &["commit", "--quiet", "-m", "base"]);
    root
}

pub struct Keys {
    tx: UnboundedSender<std::io::Result<Event>>,
    rx: UnboundedReceiverStream<std::io::Result<Event>>,
}

impl Keys {
    pub fn new() -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            tx,
            rx: UnboundedReceiverStream::new(rx),
        }
    }

    pub async fn send(&mut self, app: &mut Application, keys: &str) -> anyhow::Result<()> {
        for key in parse_macro(keys)? {
            self.tx.send(Ok(Event::Key(KeyEvent::from(key))))?;
        }
        app.event_loop_until_idle(&mut self.rx).await;
        Ok(())
    }

    /// Run the editor until background Git work satisfies `done`.
    pub async fn until(
        &mut self,
        app: &mut Application,
        what: &str,
        done: impl Fn(&Application) -> bool,
    ) {
        let start = std::time::Instant::now();
        while !done(app) {
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "timed out waiting for {what}; status: {:?}",
                app.editor.get_status()
            );
            app.event_loop_until_idle(&mut self.rx).await;
        }
    }
}

pub fn status(app: &Application) -> String {
    app.editor
        .get_status()
        .map(|(text, _)| text.to_string())
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn stage_and_commit_by_keyboard() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let root = repository(&dir.path().canonicalize()?);
    let mut app = AppBuilder::new()
        .with_file(root.join("a.txt"), None)
        .build()?;
    let mut keys = Keys::new();

    keys.send(&mut app, "ggiwell, <esc>:w<ret>").await?;
    assert_eq!(fs::read_to_string(root.join("a.txt"))?, "well, hello\n");
    keys.send(&mut app, ":git-status<ret>").await?;
    keys.until(&mut app, "the status picker", |app| {
        status(app).contains("0 staged, 1 unstaged")
    })
    .await;
    keys.send(&mut app, "<A-s>").await?;
    keys.until(&mut app, "staging", |_| {
        git(&root, &["diff", "--cached", "--name-only"]) == "a.txt"
    })
    .await;
    keys.send(&mut app, "<A-c>").await?;
    keys.until(&mut app, "the commit draft", |app| {
        !app.editor.git.drafts.is_empty()
    })
    .await;
    keys.send(&mut app, "ifix: greet politely<esc>:w<ret>")
        .await?;
    keys.until(&mut app, "the commit", |app| {
        app.editor.git.drafts.is_empty()
    })
    .await;
    assert!(status(&app).starts_with("Committed "), "{}", status(&app));
    assert_eq!(
        git(&root, &["log", "-1", "--format=%s"]),
        "fix: greet politely"
    );
    assert_eq!(git(&root, &["status", "--porcelain"]), "");

    keys.send(&mut app, ":q!<ret>").await?;
    let errors = app.close().await;
    assert!(errors.is_empty(), "{errors:?}");
    Ok(())
}
