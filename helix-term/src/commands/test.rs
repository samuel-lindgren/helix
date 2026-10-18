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
use helix_dap::{registry::DebugAdapterId, requests::RunInTerminalArguments};
use helix_view::{
    editor::{Action, TestRun, TestTerminal},
    DocumentId, Editor,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::{oneshot, watch},
};

use super::Context;
use crate::{
    compositor,
    job::Callback,
    ui::{overlay::overlaid, Picker, PickerColumn},
};

pub(crate) mod dotnet;
pub(crate) mod go;
pub(crate) mod python;

const OUTPUT_LIMIT: usize = 2 * 1024 * 1024; // per stream; keep draining after the cap
const TRUNCATED: &str =
    "\n[Output limit reached: retained the first and the last MiB of each stream.]\n";
/// Stands where the middle of a stream was left out.
const GAP: &str = "\n[… output left out here …]\n";
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
    Python,
}

fn language(editor: &Editor) -> Option<Language> {
    let extension = doc!(editor).path()?.extension()?;
    match extension.to_str()? {
        "go" => Some(Language::Go),
        "cs" | "csproj" => Some(Language::Dotnet),
        "py" => Some(Language::Python),
        _ => None,
    }
}

const UNSUPPORTED: &str = "Open a saved Go, C# or Python file to run its tests";

pub(super) fn pick(cx: &mut compositor::Context) {
    match language(cx.editor) {
        Some(Language::Go) => go::pick(cx),
        Some(Language::Dotnet) => dotnet::pick(cx, None),
        Some(Language::Python) => python::pick(cx, None),
        None => cx.editor.set_error(UNSUPPORTED),
    }
}

pub(super) fn package(cx: &mut compositor::Context) {
    match language(cx.editor) {
        Some(Language::Go) => go::package(cx),
        Some(Language::Dotnet) => dotnet::package(cx, None),
        Some(Language::Python) => python::package(cx, None),
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

/// A Python test under the debugger: the tests of the focused file's
/// directory, or what is chosen from the project's tests.
pub(super) fn debug_python(cx: &mut compositor::Context, directory: bool, debugger: Debugger) {
    if directory {
        python::package(cx, Some(debugger));
    } else {
        python::pick(cx, Some(debugger));
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
        Some(Language::Python) => python::nearest(cx),
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
    } else if title.starts_with(python::TITLE) {
        python::show_locations(cx, id);
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
    start_in(cx.editor, target, debugger);
}

/// [`start`] from where only the editor is at hand: the callback of a job
/// that found what to run.
fn start_in(editor: &mut Editor, target: TestRun, debugger: Option<Debugger>) {
    // Pickers can be reopened with last_picker; recheck at the point of launch.
    if let Err(err) =
        check_idle(editor).and_then(|()| check_workspace_saved(editor, target.workspace()))
    {
        editor.set_error(err.to_string());
        return;
    }
    if debugger.is_some() && !editor.debug_adapters.is_empty() {
        editor.set_error("Debugger is already running");
        return;
    }
    // debugpy starts the program of a Python test run itself.
    let launched = match (&target, &debugger) {
        (TestRun::Python(_), Some(debugger)) => match launch(editor, &target, debugger) {
            Ok(launched) => Some(launched),
            Err(err) => {
                editor.set_error(format!("Could not start the debugger: {err}"));
                return;
            }
        },
        _ => None,
    };
    let adapter = launched.as_ref().map(|launched| launched.adapter);
    let origin = view!(editor).id;
    let (buffer_name, header) = match &target {
        TestRun::Go(run) => (go::BUFFER_NAME, go::header(run)),
        TestRun::Dotnet(run) => (dotnet::BUFFER_NAME, dotnet::header(run, debugger.is_some())),
        TestRun::Python(run) => (python::BUFFER_NAME, python::header(run, debugger.is_some())),
    };
    let id = result_buffer(editor, buffer_name);
    replace_output(editor, id, format!("{header}RUNNING\n"));
    editor.focus(origin);
    let (cancel, rx) = watch::channel(false);
    editor.test_cancel = Some(cancel);
    editor.test_cancel_reason = None;
    editor.set_status(match debugger {
        Some(_) => format!("Starting {} for the debugger…", target.name()),
        None => format!("Running {}…", target.name()),
    });
    crate::job::spawn_callback(async move {
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
            TestRun::Python(run) => match launched {
                // Time at a breakpoint is not time the tests take.
                Some(launched) => {
                    python::run_launched(run, launched.started, rx, DEBUG_TIMEOUT).await
                }
                None => python::run(run, rx, python::RUN_TIMEOUT).await,
            },
        };
        Ok(Callback::Editor(Box::new(move |editor| {
            editor.test_cancel = None;
            if let Some(adapter) = adapter.filter(|_| !result.started) {
                stop_debugger(editor, adapter);
            }
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
struct ProcessGroup {
    pid: Option<u32>,
    /// Whether the tool leads a session of its own, whose processes can be
    /// in groups of their own: debugpy's launcher puts the tests in one.
    session: bool,
}

impl ProcessGroup {
    fn kill(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid.take() {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
            #[cfg(target_os = "linux")]
            if self.session {
                kill_session(pid);
            }
        }
    }
}

/// Every process of a session, also those in process groups of their own.
#[cfg(target_os = "linux")]
fn kill_session(session: u32) {
    let session = session.to_string();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|pid| pid.parse().ok()) else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // "pid (comm) state ppid pgrp session ...": the command name can
        // contain spaces, so the fields are counted after its parenthesis.
        let member = stat
            .rsplit_once(") ")
            .is_some_and(|(_, fields)| fields.split(' ').nth(3) == Some(session.as_str()));
        if member {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}

/// What becomes of the processes a tool has started and not stopped itself
/// when it exits. A run that is cancelled or times out is always stopped whole.
enum Leftovers {
    Killed,
    /// Among them are the tool's build servers, which make the next run
    /// faster. A process that a test started and left behind stays as well.
    Kept,
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

/// What is kept of a stream: all of it up to the limit, else its beginning
/// and its end. The beginning says what ran and the end how it went; a tool's
/// results come last.
#[derive(Default)]
struct Kept {
    head: Vec<u8>,
    tail: Vec<u8>,
    left_out: bool,
}

impl Kept {
    const HALF: usize = OUTPUT_LIMIT / 2;

    fn push(&mut self, piece: &[u8]) {
        let head = piece.len().min(Self::HALF - self.head.len());
        self.head.extend_from_slice(&piece[..head]);
        self.tail.extend_from_slice(&piece[head..]);
        // Not on every piece: moving the tail is what costs.
        if self.tail.len() > 2 * Self::HALF {
            self.trim();
        }
    }

    fn trim(&mut self) {
        if self.tail.len() > Self::HALF {
            self.tail.drain(..self.tail.len() - Self::HALF);
            self.left_out = true;
        }
    }

    /// The stream, with a mark where its middle was left out, and whether it was.
    fn finish(mut self) -> (Vec<u8>, bool) {
        self.trim();
        let mut bytes = self.head;
        let mut tail = self.tail.as_slice();
        if self.left_out {
            // Resume at the start of a line.
            if let Some(end) = tail.iter().position(|byte| *byte == b'\n') {
                tail = &tail[end + 1..];
            }
            bytes.extend_from_slice(GAP.as_bytes());
        }
        bytes.extend_from_slice(tail);
        (bytes, self.left_out)
    }
}

/// `seen` is shown each piece of the stream as it arrives, kept or not.
async fn capture(
    mut reader: impl AsyncRead + Unpin,
    kept: &mut Kept,
    mut seen: impl FnMut(&[u8]),
) -> std::io::Result<()> {
    let mut chunk = [0; 8192];
    loop {
        let len = reader.read(&mut chunk).await?;
        if len == 0 {
            return Ok(());
        }
        kept.push(&chunk[..len]);
        seen(&chunk[..len]);
    }
}

/// What a started test process left behind.
struct Execution {
    /// The reason instead, when the process was stopped before it exited.
    exit: Result<ExitStatus, String>,
    /// Both streams when they were merged.
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// Whether the middle of a stream was left out.
    truncated: bool,
}

/// How the two output streams of the process are kept.
enum Streams {
    Separate,
    /// As one stream, in the order the process wrote: for a tool that splits
    /// one report over both.
    Merged,
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

/// A started test process and the reading ends of its output.
struct Spawned {
    child: tokio::process::Child,
    group: ProcessGroup,
    merged: Option<tokio::fs::File>,
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
}

/// What of the editor's terminal the processes of a run have.
#[derive(Clone, Copy)]
enum Session {
    /// The editor's session: a process group of their own in it.
    Shared,
    /// A session of their own, without the terminal. debugpy's launcher makes
    /// the program it starts the foreground process group of the terminal it
    /// finds, and the editor would stop receiving keys.
    Own,
}

/// Starts the command in a process group of its own, with its output on pipes.
fn spawn(mut command: Command, streams: Streams, session: Session) -> std::io::Result<Spawned> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(not(unix))]
    let _ = session;
    #[cfg(unix)]
    match session {
        Session::Shared => {
            command.process_group(0);
        }
        // SAFETY: setsid is async-signal-safe, which is all `pre_exec` requires.
        // A session is a process group of the same id as well.
        Session::Own => unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        },
    }
    let merged = match streams {
        Streams::Separate => None,
        Streams::Merged => Some(merged_output(&mut command)?),
    };
    let mut child = command.spawn()?;
    let group = ProcessGroup {
        pid: child.id(),
        session: matches!(session, Session::Own),
    };
    // The command holds the writing ends of a merged pipe, which would keep
    // it open after the process has gone.
    drop(command);
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    Ok(Spawned {
        child,
        group,
        merged,
        stdout,
        stderr,
    })
}

/// Runs the command to its end, to cancellation or to the deadline, whichever
/// comes first, and stops its whole process group, unless `leftovers` keeps
/// what a tool that has exited left running. Fails only when the process
/// cannot be started. `seen` follows the standard output as it arrives.
async fn execute(
    command: Command,
    cancel: watch::Receiver<bool>,
    deadline: Duration,
    streams: Streams,
    leftovers: Leftovers,
    seen: impl FnMut(&[u8]),
) -> std::io::Result<Execution> {
    let spawned = spawn(command, streams, Session::Shared)?;
    Ok(follow(spawned, cancel, deadline, leftovers, seen).await)
}

/// Follows a started process as [`execute`] does.
async fn follow(
    spawned: Spawned,
    mut cancel: watch::Receiver<bool>,
    deadline: Duration,
    leftovers: Leftovers,
    seen: impl FnMut(&[u8]),
) -> Execution {
    let Spawned {
        mut child,
        mut group,
        merged,
        stdout,
        stderr,
    } = spawned;
    let (mut out, mut err) = (Kept::default(), Kept::default());
    let output = async {
        match (merged, stdout, stderr) {
            (Some(merged), ..) => capture(merged, &mut out, seen).await,
            (None, Some(stdout), Some(stderr)) => tokio::try_join!(
                capture(stdout, &mut out, seen),
                capture(stderr, &mut err, |_| {})
            )
            .map(|_| ()),
            _ => Ok(()),
        }
    };
    let killed = matches!(leftovers, Leftovers::Killed);
    let exited = async {
        let exit = child.wait().await?;
        // What the tool left running goes with it, and so do the ends of
        // its streams that a leftover would hold open.
        if killed {
            group.kill();
        }
        Ok(exit)
    };
    let exit = tokio::select! {
        biased;
        _ = async { if !*cancel.borrow() { let _ = cancel.changed().await; } } => Err(CANCELLED.to_owned()),
        result = tokio::time::timeout(deadline, async {
            tokio::try_join!(exited, output)
        }) => match result {
            Ok(Ok((exit, ()))) => Ok(exit),
            Ok(Err(err)) => Err(format!("PROCESS ERROR: {err}")),
            Err(_) => Err("TIMED OUT (including build time)".into()),
        }
    };
    if exit.is_ok() && !killed {
        group.pid = None;
    }
    // Drop first to stop descendants before waiting for the immediate child.
    drop(group);
    if exit.is_err() {
        let _ = child.kill().await;
    }
    let ((stdout, cut_out), (stderr, cut_err)) = (out.finish(), err.finish());
    Execution {
        exit,
        stdout,
        stderr,
        truncated: cut_out || cut_err,
    }
}

/// How long a debug adapter may take to start the program of a test run.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(60);

/// The program that a debug adapter asked for, or why it could not be started.
type Started = Result<Spawned, String>;

/// Waits for the program that a debug adapter starts for a test run, then
/// follows it as [`execute`] does. The reason instead, when no program came.
async fn follow_launched(
    started: oneshot::Receiver<Started>,
    mut cancel: watch::Receiver<bool>,
    deadline: Duration,
    leftovers: Leftovers,
    seen: impl FnMut(&[u8]),
) -> Result<Execution, String> {
    let started = tokio::select! {
        biased;
        _ = async { if !*cancel.borrow() { let _ = cancel.changed().await; } } => {
            return Err(CANCELLED.to_owned())
        }
        started = tokio::time::timeout(LAUNCH_TIMEOUT, started) => started,
    };
    match started {
        Ok(Ok(Ok(spawned))) => Ok(follow(spawned, cancel, deadline, leftovers, seen).await),
        Ok(Ok(Err(reason))) => Err(format!("COULD NOT START {reason}")),
        Ok(Err(_)) => Err("COULD NOT START: the debugger did not start the tests".into()),
        Err(_) => Err("TIMED OUT: the debugger did not start the tests".into()),
    }
}

/// The command a debug adapter asks the editor to run in a terminal.
fn requested(arguments: &RunInTerminalArguments) -> anyhow::Result<Command> {
    let (program, args) = arguments
        .args
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("the debug adapter named no program to start"))?;
    let mut command = Command::new(program);
    command.args(args);
    if !arguments.cwd.is_empty() {
        command.current_dir(&arguments.cwd);
    }
    for (name, value) in arguments.env.iter().flatten() {
        match value {
            Some(value) => command.env(name, value),
            None => command.env_remove(name),
        };
    }
    Ok(command)
}

/// The program of a debugged test run, which its debug adapter starts.
struct Launched {
    adapter: DebugAdapterId,
    started: oneshot::Receiver<Started>,
}

/// Starts the debugger on a Python test run: debugpy starts the runner
/// itself, in a terminal that the editor provides, and the run takes the
/// process over with its output.
fn launch(editor: &mut Editor, target: &TestRun, debugger: &Debugger) -> anyhow::Result<Launched> {
    let TestRun::Python(run) = target else {
        anyhow::bail!("Only the debugger of a Python test starts the tests");
    };
    let adapter = super::dap::launch_test(
        editor,
        debugger.config.clone(),
        &debugger.template,
        target.clone(),
        |args| python::debug_args(run, args),
    )?;
    let (tx, started) = oneshot::channel();
    editor.test_terminal = Some(TestTerminal {
        adapter,
        start: Box::new(move |arguments| {
            let program = arguments.args.first().cloned().unwrap_or_default();
            let started = requested(arguments).and_then(|command| {
                spawn(command, Streams::Merged, Session::Own)
                    .map_err(|err| anyhow::anyhow!("{program}: {err}"))
            });
            match started {
                Ok(spawned) => {
                    let pid = spawned.child.id().unwrap_or_default();
                    // The run may have been cancelled meanwhile; the process then goes.
                    let _ = tx.send(Ok(spawned));
                    Ok(pid)
                }
                // The adapter is told, and the run says why.
                Err(err) => {
                    let _ = tx.send(Err(err.to_string()));
                    Err(err)
                }
            }
        }),
    });
    Ok(Launched { adapter, started })
}

/// Ends the debug session of a run that its program never came to.
fn stop_debugger(editor: &mut Editor, adapter: DebugAdapterId) {
    if editor
        .test_terminal
        .as_ref()
        .is_some_and(|terminal| terminal.adapter == adapter)
    {
        editor.test_terminal = None;
    }
    if editor.debug_adapters.get_client(adapter).is_some() {
        if editor
            .debug_adapters
            .get_active_client()
            .is_some_and(|client| client.id() == adapter)
        {
            editor.debug_adapters.unset_active_client();
        }
        // The adapter's process goes with it.
        editor.debug_adapters.remove_client(adapter);
    }
}

/// The child of a syntax node in the field of that name.
fn field<'a>(
    node: &helix_core::tree_sitter::Node<'a>,
    name: &str,
) -> Option<helix_core::tree_sitter::Node<'a>> {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return None;
    }
    loop {
        if cursor.field_name() == Some(name) {
            return Some(cursor.node());
        }
        if !cursor.goto_next_sibling() {
            return None;
        }
    }
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
        app_with_languages(path, |_| {})
    }

    /// An editor with a language configuration of its own.
    pub(in crate::commands::test) fn app_with_languages(
        path: &Path,
        edit: impl FnOnce(&mut toml::Value),
    ) -> Application {
        let mut args = Args::default();
        args.files
            .insert(path.to_owned(), vec![helix_core::Position::new(0, 0)]);
        let mut config = Config::default();
        config.editor.lsp.enable = false;
        let mut languages = helix_loader::config::default_lang_config();
        edit(&mut languages);
        let syntax = helix_core::syntax::Loader::new(languages.try_into().unwrap()).unwrap();
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

    /// The session of a started process: the field after the process group
    /// in `/proc/<pid>/stat`, which `ps -o sid` shows.
    #[cfg(target_os = "linux")]
    async fn session_of(session: Session) -> (u32, String) {
        let mut command = Command::new("cat");
        command.arg("/proc/self/stat");
        let spawned = spawn(command, Streams::Merged, session).unwrap();
        let pid = spawned.child.id().unwrap();
        let (_tx, rx) = watch::channel(false);
        let execution = follow(
            spawned,
            rx,
            Duration::from_secs(10),
            Leftovers::Killed,
            |_| {},
        )
        .await;
        let stat = String::from_utf8(execution.stdout).unwrap();
        // "pid (comm) state ppid pgrp session ...": the command name can
        // contain spaces, so the fields are counted after its parenthesis.
        let (_, fields) = stat.rsplit_once(") ").unwrap();
        (pid, fields.split(' ').nth(3).unwrap().to_owned())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_run_in_a_session_of_its_own_ends_with_its_program_and_its_leftovers() {
        // With job control, a shell puts what it starts in the background in
        // a process group of its own, as debugpy's launcher does with the
        // tests. The job holds the output open.
        let mut command = Command::new("sh");
        command.args(["-c", "set -m; sleep 300 & echo $!"]);
        let spawned = spawn(command, Streams::Merged, Session::Own).unwrap();
        let (_tx, rx) = watch::channel(false);
        let run = follow(
            spawned,
            rx,
            Duration::from_secs(120),
            Leftovers::Killed,
            |_| {},
        );
        let execution = tokio::time::timeout(Duration::from_secs(10), run)
            .await
            .expect("the run ends with its program");
        assert!(execution.exit.is_ok());
        let pid: i32 = String::from_utf8(execution.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                // A killed process may remain a zombie briefly until reaped.
                let alive = unsafe { libc::kill(pid, 0) } == 0;
                let zombie = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                    .is_ok_and(|stat| stat.contains(") Z "));
                if !alive || zombie {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the leftover is gone");
    }

    #[tokio::test]
    async fn a_program_that_could_not_start_says_why() {
        let (_cancel, rx) = watch::channel(false);
        let (tx, started) = oneshot::channel();
        tx.send(Err(
            "/src/.venv/bin/python: No such file or directory".into()
        ))
        .ok()
        .unwrap();
        let reason = follow_launched(
            started,
            rx,
            Duration::from_secs(1),
            Leftovers::Killed,
            |_| {},
        )
        .await
        .err()
        .unwrap();
        assert_eq!(
            reason,
            "COULD NOT START /src/.venv/bin/python: No such file or directory"
        );
        // An adapter that went without asking for the program.
        let (_cancel, rx) = watch::channel(false);
        let (tx, started) = oneshot::channel::<Started>();
        drop(tx);
        let reason = follow_launched(
            started,
            rx,
            Duration::from_secs(1),
            Leftovers::Killed,
            |_| {},
        )
        .await
        .err()
        .unwrap();
        assert!(
            reason.starts_with("COULD NOT START: the debugger"),
            "{reason}"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_program_that_a_debug_adapter_starts_has_no_terminal_of_the_editor() {
        let (pid, session) = session_of(Session::Own).await;
        assert_eq!(session, pid.to_string());
        let (pid, session) = session_of(Session::Shared).await;
        assert_ne!(session, pid.to_string());
    }

    #[tokio::test]
    async fn capture_drains_and_keeps_both_ends() {
        // Short output is kept as it is.
        let mut kept = Kept::default();
        let mut shown = 0;
        capture(&b"first\nsecond\n"[..], &mut kept, |piece| {
            shown += piece.len()
        })
        .await
        .unwrap();
        assert_eq!(kept.finish(), (b"first\nsecond\n".to_vec(), false));
        assert_eq!(shown, 13);

        // Too much: the results at the end outlast what came before them.
        let filler = "a line of test output\n".repeat(OUTPUT_LIMIT / 10);
        let data = format!("Test run for Shop.Tests.dll\n{filler}Total tests: 3\n");
        assert!(data.len() > 2 * OUTPUT_LIMIT);
        let mut kept = Kept::default();
        let mut shown = 0;
        capture(data.as_bytes(), &mut kept, |piece| shown += piece.len())
            .await
            .unwrap();
        // What is not kept is still shown.
        assert_eq!(shown, data.len());
        let (bytes, left_out) = kept.finish();
        let text = String::from_utf8(bytes).unwrap();
        assert!(left_out);
        assert!(text.len() <= OUTPUT_LIMIT + GAP.len());
        assert!(text.starts_with("Test run for Shop.Tests.dll\n"));
        assert!(text.ends_with("a line of test output\nTotal tests: 3\n"));
        // The end resumes with a whole line.
        let (_, after) = text.split_once(GAP).unwrap();
        assert!(after.starts_with("a line of test output\n"));

        // Exactly the limit is not too much.
        let mut kept = Kept::default();
        capture(vec![b'x'; OUTPUT_LIMIT].as_slice(), &mut kept, |_| {})
            .await
            .unwrap();
        let (bytes, left_out) = kept.finish();
        assert_eq!((bytes.len(), left_out), (OUTPUT_LIMIT, false));
    }
}
