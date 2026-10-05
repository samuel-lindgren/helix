//! Test runs without a debugger. A language backend selects what to run and
//! reads the results; the process, the output buffer and the way from output
//! back to source are shared. The file in the focused view decides the
//! language.
use std::{
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    time::Duration,
};

use helix_core::{syntax::config::DebugAdapterConfig, Selection, Transaction};
use helix_view::{
    editor::{Action, TestRun},
    DocumentId, Editor,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::watch,
};

use super::Context;
use crate::{
    compositor,
    job::Callback,
    ui::{overlay::overlaid, Picker, PickerColumn},
};

pub(crate) mod dotnet;
pub(crate) mod go;

const OUTPUT_LIMIT: usize = 2 * 1024 * 1024; // per stream; keep draining after the cap
const TRUNCATED: &str = "\n[Output limit reached: retained at most 2 MiB per stream.]\n";
/// The status of a run that was stopped on request.
const CANCELLED: &str = "CANCELLED";

fn compositor_context<'a>(cx: &'a mut Context) -> compositor::Context<'a> {
    compositor::Context {
        editor: cx.editor,
        jobs: cx.jobs,
        scroll: None,
    }
}

pub fn test_picker(cx: &mut Context) {
    pick(&mut compositor_context(cx));
}

pub fn test_package(cx: &mut Context) {
    package(&mut compositor_context(cx));
}

pub fn test_nearest(cx: &mut Context) {
    nearest(&mut compositor_context(cx));
}

pub fn test_last(cx: &mut Context) {
    rerun(&mut compositor_context(cx));
}

pub fn test_results(cx: &mut Context) {
    show_results(&mut compositor_context(cx));
}

pub fn test_locations(cx: &mut Context) {
    show_locations(&mut compositor_context(cx));
}

pub fn test_cancel(cx: &mut Context) {
    cancel(&mut compositor_context(cx));
}

// The names from when Go was the only language. Keymaps that bind them keep
// working.
pub fn go_test_picker(cx: &mut Context) {
    test_picker(cx);
}

pub fn go_test_package(cx: &mut Context) {
    test_package(cx);
}

pub fn go_test_nearest(cx: &mut Context) {
    test_nearest(cx);
}

pub fn go_test_last(cx: &mut Context) {
    test_last(cx);
}

pub fn go_test_results(cx: &mut Context) {
    test_results(cx);
}

pub fn go_test_locations(cx: &mut Context) {
    test_locations(cx);
}

pub fn go_test_cancel(cx: &mut Context) {
    test_cancel(cx);
}

enum Language {
    Go,
    Dotnet,
}

fn language(editor: &Editor) -> Option<Language> {
    let extension = doc!(editor).path()?.extension()?;
    match extension.to_str()? {
        "go" => Some(Language::Go),
        "cs" | "csproj" => Some(Language::Dotnet),
        _ => None,
    }
}

const UNSUPPORTED: &str = "Open a saved Go or C# file to run its tests";

pub(super) fn pick(cx: &mut compositor::Context) {
    match language(cx.editor) {
        Some(Language::Go) => go::pick(cx),
        Some(Language::Dotnet) => dotnet::pick(cx, None),
        None => cx.editor.set_error(UNSUPPORTED),
    }
}

pub(super) fn package(cx: &mut compositor::Context) {
    match language(cx.editor) {
        Some(Language::Go) => go::package(cx),
        Some(Language::Dotnet) => dotnet::package(cx, None),
        None => cx.editor.set_error(UNSUPPORTED),
    }
}

/// The debug adapter, and the template of it, that attach to the process of
/// a test run.
#[derive(Clone)]
pub(super) struct Debugger {
    pub config: DebugAdapterConfig,
    pub template: String,
}

/// A .NET test of the focused file's project under the debugger: the whole
/// project, or what is chosen from its tests.
pub(super) fn debug_dotnet(cx: &mut compositor::Context, project: bool, debugger: Debugger) {
    if project {
        dotnet::package(cx, Some(debugger));
    } else {
        dotnet::pick(cx, Some(debugger));
    }
}

/// Runs the test again under the debugger it was debugged with.
pub(super) fn debug(cx: &mut compositor::Context, target: TestRun, debugger: Debugger) {
    start(cx, target, Some(debugger));
}

pub(super) fn nearest(cx: &mut compositor::Context) {
    match language(cx.editor) {
        Some(Language::Go) => go::nearest(cx),
        Some(Language::Dotnet) => dotnet::nearest(cx),
        None => cx.editor.set_error(UNSUPPORTED),
    }
}

pub(super) fn rerun(cx: &mut compositor::Context) {
    if let Err(err) = check_idle(cx.editor) {
        cx.editor.set_error(err.to_string());
    } else if let Some(target) = cx.editor.test_last_run.clone() {
        start_run(cx, target);
    } else {
        cx.editor
            .set_error("No test has been started in this session");
    }
}

pub(super) fn show_results(cx: &mut compositor::Context) {
    match results(cx.editor) {
        Some(id) => focus_results(cx.editor, id),
        None => cx.editor.set_error("No retained test output"),
    }
}

/// From the output buffer, jump to the source location or test on the cursor
/// line. Elsewhere, or on a line without either, pick from all reported
/// locations and failed tests.
pub(super) fn show_locations(cx: &mut compositor::Context) {
    let Some(id) = results(cx.editor) else {
        cx.editor.set_error("No retained test output");
        return;
    };
    // The output says whose it is.
    let title = cx.editor.documents[&id].text().line(0).to_string();
    if title.starts_with(dotnet::TITLE) {
        dotnet::show_locations(cx, id);
    } else {
        go::show_locations(cx, id);
    }
}

pub(super) fn cancel(cx: &mut compositor::Context) {
    if let Some(cancel) = &cx.editor.test_cancel {
        let _ = cancel.send(true);
        cx.editor.set_status("Cancelling the test run…");
    } else {
        cx.editor.set_error("No test is running");
    }
}

/// Only one run at a time: its output and its cancellation have one place each.
fn check_idle(editor: &Editor) -> anyhow::Result<()> {
    anyhow::ensure!(
        editor.test_cancel.is_none(),
        "A test is already running (Space t c to cancel)"
    );
    Ok(())
}

/// Tests read files from disk, so a modified buffer would not be part of the run.
fn check_workspace_saved(editor: &Editor, root: &Path) -> anyhow::Result<()> {
    if let Some(doc) = editor
        .documents
        .values()
        .find(|doc| doc.is_modified() && doc.path().is_some_and(|path| path.starts_with(root)))
    {
        anyhow::bail!(
            "Save modified files before running tests: {}",
            doc.display_name()
        );
    }
    Ok(())
}

/// What a backend made of a finished run.
struct RunResult {
    /// False when the process could not be started at all.
    started: bool,
    status: String,
    output: String,
    success: bool,
}

fn start_run(cx: &mut compositor::Context, target: TestRun) {
    start(cx, target, None);
}

/// With a debugger, the run waits for it in the process that runs the tests
/// and goes on once it has attached.
fn start(cx: &mut compositor::Context, target: TestRun, debugger: Option<Debugger>) {
    // Pickers can be reopened with last_picker; recheck at the point of launch.
    if let Err(err) =
        check_idle(cx.editor).and_then(|()| check_workspace_saved(cx.editor, target.workspace()))
    {
        cx.editor.set_error(err.to_string());
        return;
    }
    if debugger.is_some() && cx.editor.debug_adapters.get_active_client().is_some() {
        cx.editor.set_error("Debugger is already running");
        return;
    }
    let origin = view!(cx.editor).id;
    let (buffer_name, header) = match &target {
        TestRun::Go(run) => (go::BUFFER_NAME, go::header(run)),
        TestRun::Dotnet(run) => (dotnet::BUFFER_NAME, dotnet::header(run, debugger.is_some())),
    };
    let id = result_buffer(cx.editor, buffer_name);
    replace_output(cx.editor, id, format!("{header}RUNNING\n"));
    cx.editor.focus(origin);
    let (cancel, rx) = watch::channel(false);
    cx.editor.test_cancel = Some(cancel);
    cx.editor.test_cancel_reason = None;
    cx.editor.set_status(match debugger {
        Some(_) => format!("Starting {} for the debugger…", target.name()),
        None => format!("Running {}…", target.name()),
    });
    cx.jobs.callback(async move {
        let result = match &target {
            TestRun::Go(run) => go::run(Path::new("go"), run, rx, go::RUN_TIMEOUT).await,
            TestRun::Dotnet(run) => {
                let attach = debugger.map(|debugger| attach(debugger, target.clone()));
                // Time at a breakpoint is not time the tests take.
                let deadline = match attach {
                    Some(_) => DEBUG_TIMEOUT,
                    None => dotnet::RUN_TIMEOUT,
                };
                dotnet::run(Path::new("dotnet"), run, rx, deadline, attach).await
            }
        };
        Ok(Callback::Editor(Box::new(move |editor| {
            editor.test_cancel = None;
            let status = match editor.test_cancel_reason.take() {
                Some(reason) if result.status == CANCELLED => format!("{CANCELLED}: {reason}"),
                _ => result.status,
            };
            // A cancelled picker, unsaved-file guard, or failed process spawn
            // must not overwrite a previous runnable selection.
            if result.started {
                editor.test_last_run = Some(target);
            }
            // Closing the buffer explicitly discards it. Do not steal focus or
            // resurrect it when the process finishes.
            replace_output(editor, id, format!("{header}{status}\n\n{}", result.output));
            if result.success {
                editor.set_status(format!("{status} (Space t r for output)"));
            } else {
                editor.set_error(format!(
                    "{status} (Space t r: output, Space t f: locations)"
                ));
            }
        })))
    });
}

/// Given the id of a process that waits for a debugger.
type Attach = Box<dyn FnMut(u32) + Send>;

const DEBUG_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

/// Starts the debugger on a process of the run, from the task that reads the
/// run's output. When no debugger can be started, the run is cancelled: its
/// process would wait for one for good.
fn attach(debugger: Debugger, target: TestRun) -> Attach {
    Box::new(move |pid| {
        let (debugger, target) = (debugger.clone(), target.clone());
        tokio::spawn(crate::job::dispatch(move |editor, _| {
            let attached = super::dap::attach_to_test(
                editor,
                debugger.config,
                &debugger.template,
                pid,
                target,
            );
            if let Err(err) = attached {
                if let Some(cancel) = &editor.test_cancel {
                    let _ = cancel.send(true);
                    editor.test_cancel_reason = Some(format!("no debugger to attach ({err})"));
                }
            }
        }));
    })
}

/// The buffer with the output of the latest run, unless it has been closed.
fn results(editor: &Editor) -> Option<DocumentId> {
    editor
        .test_doc_id
        .filter(|id| editor.documents.contains_key(id))
}

fn result_buffer(editor: &mut Editor, name: &str) -> DocumentId {
    let id = match results(editor) {
        Some(id) => {
            focus_results(editor, id);
            id
        }
        None => {
            let id = editor.new_file(Action::VerticalSplit);
            editor.test_doc_id = Some(id);
            let doc = doc_mut!(editor, &id);
            doc.set_soft_wrap_override(Some(true));
            doc.readonly = true;
            id
        }
    };
    doc_mut!(editor, &id).set_virtual_name(Some(name.into()));
    id
}

fn focus_results(editor: &mut Editor, id: DocumentId) {
    let visible = editor
        .tree
        .traverse()
        .find(|(_, view)| view.doc == id)
        .map(|(id, _)| id);
    if let Some(view) = visible {
        editor.focus(view);
    } else {
        editor.switch(id, Action::VerticalSplit);
    }
}

fn replace_output(editor: &mut Editor, id: DocumentId, output: String) {
    let view_id = editor
        .tree
        .traverse()
        .find(|(_, v)| v.doc == id)
        .map(|(id, _)| id)
        .unwrap_or_else(|| view!(editor).id);
    let Some(doc) = editor.documents.get_mut(&id) else {
        return;
    };
    // Also update hidden buffers. A view may hold selections/jumps for documents
    // other than its current one; committing updates only this document's jumps.
    doc.ensure_view_init(view_id);
    let transaction = Transaction::change(
        doc.text(),
        std::iter::once((0, doc.text().len_chars(), Some(output.into()))),
    )
    .with_selection(Selection::point(0));
    doc.apply(&transaction, view_id);
    doc.append_changes_to_history(editor.tree.get_mut(view_id));
    doc.reset_modified();
}

// Kill the process group as well as the tool, so cancellation/editor exit also
// stops the test executable and compiler children on Unix. kill_on_drop covers
// the tool itself on all platforms. The group is private to this invocation.
struct ProcessGroup(Option<u32>);

/// What becomes of the processes a tool has started and not stopped itself
/// when it exits. A run that is cancelled or times out is always stopped whole.
enum Leftovers {
    Killed,
    /// They are the tool's build servers, which make the next run faster.
    Kept,
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.0 {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
}

/// `seen` is shown all that has been kept, each time there is more of it.
async fn capture(
    mut reader: impl AsyncRead + Unpin,
    bytes: &mut Vec<u8>,
    mut seen: impl FnMut(&[u8]),
) -> std::io::Result<bool> {
    let mut truncated = false;
    let mut chunk = [0; 8192];
    loop {
        let len = reader.read(&mut chunk).await?;
        if len == 0 {
            return Ok(truncated);
        }
        let keep = len.min(OUTPUT_LIMIT - bytes.len());
        bytes.extend_from_slice(&chunk[..keep]);
        truncated |= keep < len;
        if keep > 0 {
            seen(bytes);
        }
    }
}

/// What a started test process left behind.
struct Execution {
    /// The reason instead, when the process was stopped before it exited.
    exit: Result<ExitStatus, String>,
    /// Both streams when they were merged.
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// How the two output streams of the process are kept.
enum Streams {
    Separate,
    /// As one stream, in the order the process wrote: for a tool that splits
    /// one report over both.
    Merged,
}

impl Execution {
    fn truncated(&self) -> bool {
        self.stdout.len() >= OUTPUT_LIMIT || self.stderr.len() >= OUTPUT_LIMIT
    }
}

/// The reading end of a pipe that the process writes both of its streams to.
fn merged_output(command: &mut Command) -> std::io::Result<tokio::fs::File> {
    let (reader, writer) = std::io::pipe()?;
    command.stdout(writer.try_clone()?).stderr(writer);
    #[cfg(unix)]
    let reader = std::fs::File::from(std::os::fd::OwnedFd::from(reader));
    #[cfg(windows)]
    let reader = std::fs::File::from(std::os::windows::io::OwnedHandle::from(reader));
    Ok(tokio::fs::File::from_std(reader))
}

/// Runs the command to its end, to cancellation or to the deadline, whichever
/// comes first, and stops its whole process group. Fails only when the process
/// cannot be started. `seen` follows the standard output as it arrives.
async fn execute(
    mut command: Command,
    mut cancel: watch::Receiver<bool>,
    deadline: Duration,
    streams: Streams,
    leftovers: Leftovers,
    seen: impl FnMut(&[u8]),
) -> std::io::Result<Execution> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let merged = match streams {
        Streams::Separate => None,
        Streams::Merged => Some(merged_output(&mut command)?),
    };
    let mut child = command.spawn()?;
    let mut group = ProcessGroup(child.id());
    // The command holds the writing ends of a merged pipe, which would keep
    // it open after the process has gone.
    drop(command);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let output = async {
        match (merged, stdout, stderr) {
            (Some(merged), ..) => capture(merged, &mut out, seen).await.map(|_| ()),
            (None, Some(stdout), Some(stderr)) => tokio::try_join!(
                capture(stdout, &mut out, seen),
                capture(stderr, &mut err, |_| {})
            )
            .map(|_| ()),
            _ => Ok(()),
        }
    };
    let exit = tokio::select! {
        biased;
        _ = async { if !*cancel.borrow() { let _ = cancel.changed().await; } } => Err(CANCELLED.to_owned()),
        result = tokio::time::timeout(deadline, async {
            tokio::try_join!(child.wait(), output)
        }) => match result {
            Ok(Ok((exit, ()))) => Ok(exit),
            Ok(Err(err)) => Err(format!("PROCESS ERROR: {err}")),
            Err(_) => Err("TIMED OUT (including build time)".into()),
        }
    };
    if exit.is_ok() && matches!(leftovers, Leftovers::Kept) {
        group.0 = None;
    }
    // Drop first to stop descendants before waiting for the immediate child.
    drop(group);
    if exit.is_err() {
        let _ = child.kill().await;
    }
    Ok(Execution {
        exit,
        stdout: out,
        stderr: err,
    })
}

/// A line of a source file that the output of a run points to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SourceLocation {
    path: PathBuf,
    line: usize,
    message: String,
}

fn pick_location(cx: &mut compositor::Context, locations: Vec<SourceLocation>) {
    cx.jobs.callback(async move {
        Ok(Callback::EditorCompositor(Box::new(
            move |_, compositor| {
                let picker = Picker::new(
                    [
                        PickerColumn::new("file", |l: &SourceLocation, _| {
                            l.path.to_string_lossy().into_owned().into()
                        }),
                        PickerColumn::new("line", |l: &SourceLocation, _| {
                            (l.line + 1).to_string().into()
                        }),
                        PickerColumn::new("message", |l: &SourceLocation, _| {
                            l.message.as_str().into()
                        }),
                    ],
                    2,
                    locations,
                    (),
                    |cx, location, action| jump_to_location(cx.editor, location, action),
                )
                .with_preview(|_, location| {
                    Some((
                        location.path.as_path().into(),
                        Some((location.line, location.line)),
                    ))
                });
                compositor.push(Box::new(overlaid(picker)));
            },
        )))
    });
}

fn jump_to_location(editor: &mut Editor, location: &SourceLocation, action: Action) {
    if !location.path.is_file() {
        editor.set_error("Reported source file no longer exists");
        return;
    }
    // Keep the output on screen: open sources in the split beside it.
    if matches!(action, Action::Replace) && Some(view!(editor).doc) == editor.test_doc_id {
        if let Some(view) = source_view(editor) {
            editor.focus(view);
        }
    }
    let id = match editor.open(&location.path, action) {
        Ok(id) => id,
        Err(err) => {
            editor.set_error(err.to_string());
            return;
        }
    };
    let view = view_mut!(editor);
    let doc = doc_mut!(editor, &id);
    if location.line >= doc.text().len_lines() {
        editor.set_error("Reported line no longer exists; save and rerun the test");
        return;
    }
    doc.set_selection(
        view.id,
        Selection::point(doc.text().line_to_char(location.line)),
    );
    if action.align_view(view, id) {
        super::align_view(doc, view, super::Align::Center);
    }
}

/// The most recently focused split that is not showing the test output.
fn source_view(editor: &Editor) -> Option<helix_view::ViewId> {
    editor
        .tree
        .views()
        .map(|(view, _)| view)
        .filter(|view| Some(view.doc) != editor.test_doc_id)
        .max_by_key(|view| editor.documents[&view.doc].focused_at)
        .map(|view| view.id)
}

/// Drives an editor through the commands, for the tests of the backends.
#[cfg(all(test, feature = "integration"))]
mod harness {
    use std::path::{Path, PathBuf};

    use helix_core::Selection;

    use super::*;
    use crate::{application::Application, args::Args, config::Config};

    pub(in crate::commands::test) async fn keys(app: &mut Application, input: &str) {
        #[cfg(windows)]
        use crossterm::event::{Event, KeyEvent};
        #[cfg(not(windows))]
        use termina::event::{Event, KeyEvent};
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        for key in helix_view::input::parse_macro(input).unwrap() {
            tx.send(Ok(Event::Key(KeyEvent::from(key)))).unwrap();
        }
        let mut stream = tokio_stream::wrappers::UnboundedReceiverStream::new(rx);
        assert!(tokio::time::timeout(
            Duration::from_secs(30),
            app.event_loop_until_idle(&mut stream)
        )
        .await
        .unwrap());
    }

    pub(in crate::commands::test) fn test_app(path: &Path) -> Application {
        let mut args = Args::default();
        args.files
            .insert(path.to_owned(), vec![helix_core::Position::new(0, 0)]);
        let mut config = Config::default();
        config.editor.lsp.enable = false;
        let syntax = helix_core::syntax::Loader::new(
            helix_loader::config::default_lang_config()
                .try_into()
                .unwrap(),
        )
        .unwrap();
        Application::new(args, config, syntax).unwrap()
    }

    pub(in crate::commands::test) async fn finished_output(app: &mut Application) -> String {
        // A first .NET build restores its packages.
        tokio::time::timeout(Duration::from_secs(180), async {
            while app.editor.test_cancel.is_some() {
                keys(app, "").await;
            }
        })
        .await
        .unwrap();
        let id = app.editor.test_doc_id.unwrap();
        app.editor.documents[&id].text().to_string()
    }

    /// Put the cursor on the first output line containing `needle` and run
    /// Space t f, as a user reading the results would.
    pub(in crate::commands::test) async fn follow(app: &mut Application, needle: &str) {
        let id = app.editor.test_doc_id.unwrap();
        focus_results(&mut app.editor, id);
        let (view, doc) = current!(app.editor);
        let line = doc
            .text()
            .lines()
            .position(|line| line.to_string().contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} not in output:\n{}", doc.text()));
        doc.set_selection(view.id, Selection::point(doc.text().line_to_char(line)));
        keys(app, "<space>tf").await;
    }

    /// The focused file and 1-based cursor line.
    pub(in crate::commands::test) fn position(app: &Application) -> (PathBuf, usize) {
        let (view, doc) = current_ref!(app.editor);
        let line = doc
            .selection(view.id)
            .primary()
            .cursor_line(doc.text().slice(..));
        (doc.path().cloned().unwrap_or_default(), line + 1)
    }

    pub(in crate::commands::test) fn status(app: &Application) -> String {
        app.editor
            .get_status()
            .map(|(status, _)| status.to_string())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn capture_drains_but_bounds_output() {
        let data = vec![b'x'; OUTPUT_LIMIT + 123];
        let mut saved = Vec::new();
        let mut shown = Vec::new();
        assert!(
            capture(data.as_slice(), &mut saved, |all| shown.push(all.len()))
                .await
                .unwrap()
        );
        assert_eq!(saved.len(), OUTPUT_LIMIT);
        // Shown as it grows, and not again once nothing more is kept.
        assert!(shown.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(shown.last(), Some(&OUTPUT_LIMIT));
    }
}
