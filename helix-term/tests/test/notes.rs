//! Project notes driven by keys: capture a task from code, save it under the
//! heading of the project's notes file, move the code, go back to it from the
//! task picker and mark the task done.
use std::fs;

use helix_term::application::Application;

use super::git::{status, Keys};
use super::*;

fn cursor_line(app: &Application) -> usize {
    let (view, doc) = helix_view::current_ref!(app.editor);
    let text = doc.text().slice(..);
    text.char_to_line(doc.selection(view.id).primary().cursor(text))
}

#[tokio::test(flavor = "multi_thread")]
async fn capture_list_and_return_to_code() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let dir = dir.path().canonicalize()?;
    let project = dir.join("app");
    fs::create_dir_all(project.join(".helix"))?;
    let source = project.join("main.txt");
    fs::write(&source, "one\ntwo\n  the line\nfour\n")?;
    let notes = dir.join("notes");

    let mut config = test_config();
    config.editor.notes.directory = notes.clone();
    let mut app = AppBuilder::new()
        .with_file(source.clone(), None)
        .with_config(config)
        .build()?;
    let mut keys = Keys::new();

    keys.send(&mut app, "3gg<space>nc").await?;
    assert!(
        status(&app).starts_with("Capturing main.txt:3"),
        "{}",
        status(&app)
    );
    keys.send(&mut app, "Check the line<esc>:w<ret>").await?;
    assert!(
        status(&app).starts_with("Saved under \"Inbox\""),
        "{}",
        status(&app)
    );
    let file = notes.join("app.md");
    assert_eq!(
        fs::read_to_string(&file)?,
        format!(
            "---\nproject: {}\n---\n# app\n\n## Inbox\n\n- [ ] Check the line\n  code: main.txt:3\n  > the line\n",
            project.display()
        )
    );
    assert!(app.editor.notes.captures.is_empty());
    assert_eq!(
        helix_view::doc!(app.editor).path(),
        Some(&source),
        "the draft closes and the code is back in focus"
    );

    // From the code: toggle the linked task and go to its entry.
    keys.send(&mut app, "<space>nx").await?;
    assert_eq!(status(&app), "Done: Check the line");
    assert!(fs::read_to_string(&file)?.contains("- [x] Check the line\n"));
    keys.send(&mut app, "<space>nx").await?;
    assert_eq!(status(&app), "Reopened: Check the line");
    keys.send(&mut app, "gg<space>nx").await?;
    assert!(
        status(&app).starts_with("No task links to this line"),
        "{}",
        status(&app)
    );
    keys.send(&mut app, "3gg<space>ng").await?;
    assert_eq!(helix_view::doc!(app.editor).path(), Some(&file));
    assert_eq!(cursor_line(&app), 7);
    keys.send(&mut app, "<C-o>").await?;
    assert_eq!(helix_view::doc!(app.editor).path(), Some(&source));

    // Two lines above the anchor move it from line 3 to line 5.
    keys.send(&mut app, "ggOzero<ret>zero<esc>:w<ret>").await?;
    keys.send(&mut app, "<space>nt").await?;
    keys.send(&mut app, "<ret>").await?;
    assert_eq!(helix_view::doc!(app.editor).path(), Some(&source));
    assert_eq!(cursor_line(&app), 4);
    assert!(status(&app).contains("line 3 is now 5"), "{}", status(&app));

    keys.send(&mut app, "<space>nt").await?;
    keys.send(&mut app, "<A-x>").await?;
    assert!(
        fs::read_to_string(&file)?.contains("- [x] Check the line\n"),
        "{}",
        status(&app)
    );
    assert_eq!(status(&app), "Done: Check the line");
    keys.send(&mut app, "<space>nt").await?;
    assert_eq!(status(&app), "No open tasks in this project");

    // From the notes buffer: the entry's link leads to the code.
    keys.send(&mut app, "<space>nn/the line<ret><space>ng")
        .await?;
    assert_eq!(helix_view::doc!(app.editor).path(), Some(&source));
    assert_eq!(cursor_line(&app), 4);

    keys.send(&mut app, ":q!<ret>").await?;
    let errors = app.close().await;
    assert!(errors.is_empty(), "{errors:?}");
    Ok(())
}

/// Captures name the innermost definition from `tags.scm`, whether the query
/// captures the whole definition (Go) or only its name (Rust), and the symbol
/// leads back when the anchored line changed.
#[tokio::test(flavor = "multi_thread")]
async fn symbols_name_and_find_the_code() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let dir = dir.path().canonicalize()?;
    let project = dir.join("svc");
    fs::create_dir_all(project.join(".helix"))?;
    let go = project.join("client.go");
    fs::write(
        &go,
        "package main\n\ntype Client struct{}\n\nfunc (c *Client) do() {\n\tx := 1\n\t_ = x\n}\n",
    )?;
    let rust = project.join("lib.rs");
    fs::write(
        &rust,
        "mod inner {\n    fn run() {\n        let a = 1;\n    }\n}\n",
    )?;
    let notes = dir.join("notes");

    let mut config = test_config();
    config.editor.notes.directory = notes.clone();
    let mut app = AppBuilder::new()
        .with_file(go.clone(), None)
        .with_config(config)
        .build()?;
    let mut keys = Keys::new();

    keys.send(&mut app, "6gg<space>nc").await?;
    keys.send(&mut app, "Go task<esc>:w<ret>").await?;
    keys.send(&mut app, &format!(":o {}<ret>", rust.display()))
        .await?;
    keys.send(&mut app, "3gg<space>nc").await?;
    keys.send(&mut app, "Rust note<esc>:w<ret>").await?;
    let text = fs::read_to_string(notes.join("svc.md"))?;
    assert!(
        text.contains("- [ ] Go task\n  code: client.go:6 `do`\n  > x := 1\n"),
        "{text}"
    );
    assert!(
        text.contains("- [ ] Rust note\n  code: lib.rs:3 `run`\n  > let a = 1;\n"),
        "{text}"
    );

    // The anchored line changes; the symbol still finds the code.
    fs::write(
        &go,
        "package main\n\ntype Client struct{}\n\n// do does it.\nfunc (c *Client) do() {\n\ty := 2\n\t_ = y\n}\n",
    )?;
    keys.send(&mut app, ":reload-all<ret><space>nn/Go task<ret><space>ng")
        .await?;
    assert_eq!(helix_view::doc!(app.editor).path(), Some(&go));
    assert_eq!(cursor_line(&app), 5);
    assert!(status(&app).contains("went to `do`"), "{}", status(&app));

    keys.send(&mut app, ":q!<ret>").await?;
    let errors = app.close().await;
    assert!(errors.is_empty(), "{errors:?}");
    Ok(())
}
