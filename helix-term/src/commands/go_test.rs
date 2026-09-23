//! A deliberately small, package-local Go test runner. Discovery is shared with
//! DAP; execution, output and cancellation are independent of the debugger.
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use helix_core::{regex::Regex, Selection, Transaction};
use helix_view::{
    editor::{Action, GoTestRun},
    DocumentId, Editor,
};
use once_cell::sync::Lazy;
use serde::Deserialize;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::watch,
};

use super::{
    dap::{find_go_tests_in_dir, go_test_run_regex, GoTestEntry},
    Context,
};
use crate::{
    compositor,
    job::Callback,
    ui::{overlay::overlaid, Picker, PickerColumn},
};

mod cursor;

const OUTPUT_LIMIT: usize = 2 * 1024 * 1024; // per stream; keep draining after the cap
const RUN_TIMEOUT: Duration = Duration::from_secs(180); // includes compilation

pub fn go_test_picker(cx: &mut Context) {
    pick(&mut compositor::Context {
        editor: cx.editor,
        jobs: cx.jobs,
        scroll: None,
    });
}

pub fn go_test_nearest(cx: &mut Context) {
    nearest(&mut compositor::Context {
        editor: cx.editor,
        jobs: cx.jobs,
        scroll: None,
    });
}

pub(super) fn nearest(cx: &mut compositor::Context) {
    if cx.editor.go_test_cancel.is_some() {
        cx.editor
            .set_error("A Go test is already running (Space t c to cancel)");
        return;
    }
    let target = (|| -> anyhow::Result<_> {
        let (view, doc) = current_ref!(cx.editor);
        let path = doc
            .path()
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with("_test.go"))
            })
            .ok_or_else(|| anyhow::anyhow!("Open a saved Go test file (*_test.go)"))?;
        let dir = path.parent().unwrap().to_owned();
        check_saved(cx.editor, &dir)?;
        anyhow::ensure!(
            doc.language_name() == Some("go"),
            "Set the document language to Go first"
        );
        let syntax = doc.syntax().ok_or_else(|| {
            anyhow::anyhow!("Go syntax is unavailable; install the Go grammar or use Space t t")
        })?;
        let byte = doc.text().char_to_byte(
            doc.selection(view.id)
                .primary()
                .cursor(doc.text().slice(..)),
        );
        let target = cursor::at_cursor(
            syntax.tree().root_node(),
            &doc.text().to_string(),
            byte,
            path.file_name().unwrap().to_str().unwrap(),
        )?;
        Ok((dir, target))
    })();
    match target {
        Ok((dir, target)) => start(cx, dir, target.entry, target.note),
        Err(err) => cx.editor.set_error(err.to_string()),
    }
}

pub fn go_test_last(cx: &mut Context) {
    rerun(&mut compositor::Context {
        editor: cx.editor,
        jobs: cx.jobs,
        scroll: None,
    });
}

pub(super) fn rerun(cx: &mut compositor::Context) {
    if cx.editor.go_test_cancel.is_some() {
        cx.editor
            .set_error("A Go test is already running (Space t c to cancel)");
    } else if let Some(target) = cx.editor.go_test_last_run.clone() {
        start_run(cx, target);
    } else {
        cx.editor
            .set_error("No Go test has been started in this session");
    }
}

pub fn go_test_results(cx: &mut Context) {
    show_results(&mut compositor::Context {
        editor: cx.editor,
        jobs: cx.jobs,
        scroll: None,
    });
}

pub fn go_test_locations(cx: &mut Context) {
    show_locations(&mut compositor::Context {
        editor: cx.editor,
        jobs: cx.jobs,
        scroll: None,
    });
}

pub fn go_test_cancel(cx: &mut Context) {
    cancel(&mut compositor::Context {
        editor: cx.editor,
        jobs: cx.jobs,
        scroll: None,
    });
}

fn workspace_root(dir: &Path) -> PathBuf {
    dir.ancestors()
        .find(|p| p.join("go.work").is_file())
        .or_else(|| dir.ancestors().find(|p| p.join("go.mod").is_file()))
        .unwrap_or(dir)
        .to_owned()
}

fn check_saved(editor: &Editor, dir: &Path) -> anyhow::Result<()> {
    check_workspace_saved(editor, &workspace_root(dir))
}

fn check_workspace_saved(editor: &Editor, root: &Path) -> anyhow::Result<()> {
    if let Some(doc) = editor
        .documents
        .values()
        .find(|doc| doc.is_modified() && doc.path().is_some_and(|path| path.starts_with(root)))
    {
        anyhow::bail!(
            "Save modified files before running Go tests: {}",
            doc.display_name()
        );
    }
    Ok(())
}

pub(super) fn pick(cx: &mut compositor::Context) {
    if cx.editor.go_test_cancel.is_some() {
        cx.editor
            .set_error("A Go test is already running (Space t c to cancel)");
        return;
    }
    let Some(path) = doc!(cx.editor)
        .path()
        .filter(|p| p.extension().is_some_and(|e| e == "go"))
    else {
        cx.editor.set_error("Open a saved Go file to select a test");
        return;
    };
    let dir = path.parent().unwrap().to_owned();
    if let Err(err) = check_saved(cx.editor, &dir) {
        cx.editor.set_error(err.to_string());
        return;
    }
    let tests = find_go_tests_in_dir(&dir);
    if tests.is_empty() {
        cx.editor.set_error("No Go tests found in this package");
        return;
    }
    cx.jobs.callback(async move {
        Ok(Callback::EditorCompositor(Box::new(
            move |_, compositor| {
                let picker = Picker::new(
                    [
                        PickerColumn::new("test", |t: &GoTestEntry, _| t.name.as_str().into()),
                        PickerColumn::new("case", |t: &GoTestEntry, _| {
                            t.subtest.as_deref().unwrap_or("").into()
                        }),
                        PickerColumn::new("file", |t: &GoTestEntry, _| t.file.as_str().into()),
                    ],
                    0,
                    tests,
                    (),
                    move |cx, entry, _| start(cx, dir.clone(), entry.clone(), None),
                );
                compositor.push(Box::new(overlaid(picker)));
            },
        )))
    });
}

fn selection(dir: &Path, entry: &GoTestEntry, note: Option<String>) -> GoTestRun {
    GoTestRun {
        directory: dir.to_owned(),
        workspace: workspace_root(dir),
        name: test_name(entry),
        run_pattern: go_test_run_regex(entry),
        selection_note: note,
    }
}

fn start(cx: &mut compositor::Context, dir: PathBuf, entry: GoTestEntry, note: Option<String>) {
    start_run(cx, selection(&dir, &entry, note));
}

fn start_run(cx: &mut compositor::Context, target: GoTestRun) {
    // Pickers can be reopened with last_picker; recheck at the point of launch.
    if cx.editor.go_test_cancel.is_some() {
        cx.editor
            .set_error("A Go test is already running (Space t c to cancel)");
        return;
    }
    if let Err(err) = check_workspace_saved(cx.editor, &target.workspace) {
        cx.editor.set_error(err.to_string());
        return;
    }
    let origin = view!(cx.editor).id;
    let id = result_buffer(cx.editor);
    let notice = target
        .selection_note
        .as_ref()
        .map(|note| format!("Original selection: {note}\n"))
        .unwrap_or_default();
    let header = format!(
        "Go test: {}\nPackage: {}\nCommand: go test -json -count=1 -timeout=2m -run {:?} .\n\
         Tests read saved files from disk. Save and rerun after edits.\n\
         Space t f: source locations | Space t r: results | Space t c: cancel\n{notice}\n",
        target.name,
        target.directory.display(),
        target.run_pattern
    );
    replace_output(cx.editor, id, format!("{header}RUNNING\n"));
    cx.editor.focus(origin);
    let (cancel, rx) = watch::channel(false);
    cx.editor.go_test_cancel = Some(cancel);
    cx.editor.set_status(format!("Running {}…", target.name));
    cx.jobs.callback(async move {
        let result = run(Path::new("go"), &target, rx, RUN_TIMEOUT).await;
        let output = format!("{header}{}\n\n{}", result.status, result.output);
        Ok(Callback::Editor(Box::new(move |editor| {
            editor.go_test_cancel = None;
            // A cancelled picker, unsaved-file guard, or failed process spawn
            // must not overwrite a previous runnable selection.
            if result.started {
                editor.go_test_last_run = Some(target);
            }
            // Closing the buffer explicitly discards it. Do not steal focus or
            // resurrect it when the process finishes.
            replace_output(editor, id, output);
            if result.success {
                editor.set_status(format!("{} (Space t r for output)", result.status));
            } else {
                editor.set_error(format!(
                    "{} (Space t r: output, Space t f: locations)",
                    result.status
                ));
            }
        })))
    });
}

fn result_buffer(editor: &mut Editor) -> DocumentId {
    if let Some(id) = editor
        .go_test_doc_id
        .filter(|id| editor.documents.contains_key(id))
    {
        focus_results(editor, id);
        return id;
    }
    let id = editor.new_file(Action::VerticalSplit);
    editor.go_test_doc_id = Some(id);
    let doc = doc_mut!(editor, &id);
    doc.set_virtual_name(Some("[go-test]".into()));
    doc.set_soft_wrap_override(Some(true));
    doc.readonly = true;
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

pub(super) fn show_results(cx: &mut compositor::Context) {
    match cx
        .editor
        .go_test_doc_id
        .filter(|id| cx.editor.documents.contains_key(id))
    {
        Some(id) => focus_results(cx.editor, id),
        None => cx.editor.set_error("No retained Go test output"),
    }
}

pub(super) fn cancel(cx: &mut compositor::Context) {
    if let Some(cancel) = &cx.editor.go_test_cancel {
        let _ = cancel.send(true);
        cx.editor.set_status("Cancelling Go test…");
    } else {
        cx.editor.set_error("No Go test is running");
    }
}

fn test_name(entry: &GoTestEntry) -> String {
    match &entry.subtest {
        Some(subtest) => format!(
            "{}/{}",
            entry.name,
            subtest
                .chars()
                .map(|c| if c.is_ascii_whitespace() { '_' } else { c })
                .collect::<String>()
        ),
        None => entry.name.clone(),
    }
}

struct RunResult {
    started: bool,
    status: String,
    output: String,
    success: bool,
}

// Kill the process group as well as `go`, so cancellation/editor exit also stops
// the test executable and compiler children on Unix. kill_on_drop covers `go`
// itself on all platforms. The group is private to this invocation.
struct ProcessGroup(Option<u32>);
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

async fn capture(mut reader: impl AsyncRead + Unpin, bytes: &mut Vec<u8>) -> std::io::Result<bool> {
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
    }
}

async fn run(
    program: &Path,
    target: &GoTestRun,
    mut cancel: watch::Receiver<bool>,
    deadline: Duration,
) -> RunResult {
    let dir = &target.directory;
    let mut command = Command::new(program);
    command
        .args([
            "test",
            "-json",
            "-count=1",
            "-timeout=2m",
            "-run",
            &target.run_pattern,
            ".",
        ])
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            return RunResult {
                started: false,
                status: "COULD NOT START go test".into(),
                output: format!("{err}\n"),
                success: false,
            }
        }
    };
    let group = ProcessGroup(child.id());
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let result = tokio::select! {
        biased;
        _ = async { if !*cancel.borrow() { let _ = cancel.changed().await; } } => Err("CANCELLED".to_owned()),
        result = tokio::time::timeout(deadline, async {
            tokio::try_join!(child.wait(), capture(stdout, &mut out), capture(stderr, &mut err))
        }) => match result {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(err)) => Err(format!("PROCESS ERROR: {err}")),
            Err(_) => Err("TIMED OUT (including build time)".into()),
        }
    };
    // Drop first to stop descendants before waiting for the immediate child.
    drop(group);
    if result.is_err() {
        let _ = child.kill().await;
    }
    let parsed = parse_output(&out, &target.name, dir);
    let mut output = parsed.output;
    if !err.is_empty() {
        output.push_str("\n--- stderr / build diagnostics ---\n");
        output.push_str(&resolve_locations(&String::from_utf8_lossy(&err), dir));
    }
    if out.len() >= OUTPUT_LIMIT || err.len() >= OUTPUT_LIMIT {
        output.push_str("\n[Output limit reached: retained at most 2 MiB per stream.]\n");
    }
    let status = match result {
        Err(reason) => reason,
        Ok((exit, _, _)) => {
            if !exit.success() {
                format!("FAILED ({exit})")
            } else if !parsed.ran {
                "NOT RUN: selected test was not observed (check build tags, names, or truncated output)".into()
            } else if parsed.skipped {
                "SKIPPED: selected test did not pass or fail".into()
            } else if !parsed.passed {
                "INCOMPLETE: no passing result observed for the selected test".into()
            } else {
                "PASSED".into()
            }
        }
    };
    RunResult {
        started: true,
        success: status == "PASSED",
        status,
        output,
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct TestEvent {
    #[serde(default)]
    action: String,
    #[serde(default)]
    test: String,
    #[serde(default)]
    output: String,
}
#[derive(Default)]
struct ParsedOutput {
    output: String,
    ran: bool,
    skipped: bool,
    passed: bool,
}

fn parse_output(bytes: &[u8], target: &str, dir: &Path) -> ParsedOutput {
    let mut parsed = ParsedOutput::default();
    for line in String::from_utf8_lossy(bytes).split_inclusive('\n') {
        if let Ok(event) = serde_json::from_str::<TestEvent>(line) {
            if event.test == target {
                parsed.ran |= event.action == "run";
                parsed.skipped |= event.action == "skip";
                parsed.passed |= event.action == "pass";
            }
            parsed
                .output
                .push_str(&resolve_locations(&event.output, dir));
        } else {
            // Older Go versions emit build diagnostics outside the JSON stream.
            parsed.output.push_str(&resolve_locations(line, dir));
        }
    }
    parsed
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SourceLocation {
    path: PathBuf,
    line: usize,
    message: String,
}
static LOCATION: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^\s*(.*\.go):([0-9]+)(?::[0-9]+)?(?::|\s|$)(.*)").unwrap());

fn source_location(line: &str, dir: &Path) -> Option<SourceLocation> {
    let captures = LOCATION.captures(line)?;
    let line = captures[2].parse::<usize>().ok()?.checked_sub(1)?;
    let path = helix_stdx::path::canonicalize(dir.join(&captures[1]));
    Some(SourceLocation {
        path,
        line,
        message: captures[3].trim().to_owned(),
    })
}

fn resolve_locations(output: &str, dir: &Path) -> String {
    let mut result = String::new();
    for line in output.split_inclusive('\n') {
        if let Some(location) = source_location(line.trim_end_matches('\n'), dir) {
            // Keep the original diagnostic (including its column) intact.
            let captures = LOCATION.captures(line.trim_end_matches('\n')).unwrap();
            let path = captures.get(1).unwrap();
            result.push_str(&line[..path.start()]);
            result.push_str(&location.path.to_string_lossy());
            result.push_str(&line[path.end()..]);
        } else {
            result.push_str(line);
        }
    }
    result
}

pub(super) fn show_locations(cx: &mut compositor::Context) {
    let Some(doc) = cx
        .editor
        .go_test_doc_id
        .and_then(|id| cx.editor.documents.get(&id))
    else {
        cx.editor.set_error("No retained Go test output");
        return;
    };
    let mut locations = Vec::new();
    let mut seen = HashSet::new();
    for line in doc.text().lines() {
        if let Some(location) = source_location(line.to_string().trim_end(), Path::new("")) {
            if seen.insert(location.clone()) {
                locations.push(location);
            }
        }
    }
    if locations.is_empty() {
        cx.editor
            .set_error("No source locations reported; Space t r shows the full output");
        return;
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, subtest: Option<&str>) -> GoTestEntry {
        GoTestEntry {
            name: name.into(),
            subtest: subtest.map(str::to_owned),
            file: "sample_test.go".into(),
        }
    }

    #[test]
    fn json_requires_the_selected_test_and_retains_diagnostics() {
        let dir = std::env::temp_dir();
        let data = br#"{"Action":"run","Test":"TestFoo"}
{"Action":"run","Test":"TestFoo/other"}
{"Action":"pass","Test":"TestFoo/other"}
{"Action":"output","Test":"TestFoo/other","Output":"    sample_test.go:12: expected 42, got 0\n"}
{"Action":"pass","Test":"TestFoo"}
"#;
        let missing = parse_output(data, "TestFoo/chosen", &dir);
        assert!(!missing.ran && !missing.passed);
        let actual = parse_output(data, "TestFoo/other", &dir);
        assert!(actual.ran && actual.passed && !actual.skipped);
        assert!(actual.output.contains("expected 42, got 0"));
        assert!(actual
            .output
            .contains(&dir.join("sample_test.go").to_string_lossy().to_string()));
        let skipped = parse_output(br#"{"Action":"skip","Test":"TestFoo"}"#, "TestFoo", &dir);
        assert!(skipped.skipped && !skipped.passed);
    }

    #[test]
    fn locations_cover_compiler_assertions_and_stack_traces() {
        let dir = std::env::temp_dir().join("package with spaces");
        for (text, file, line, message) in [
            (
                "    foo_test.go:12: expected value",
                "foo_test.go",
                11,
                "expected value",
            ),
            (
                "./foo.go:3:8: undefined: value",
                "foo.go",
                2,
                "undefined: value",
            ),
            (
                "\tfile with spaces.go:42 +0xff",
                "file with spaces.go",
                41,
                "+0xff",
            ),
        ] {
            let location = source_location(text, &dir).unwrap();
            assert_eq!(
                location.path,
                helix_stdx::path::canonicalize(dir.join(file))
            );
            assert_eq!(location.line, line);
            assert_eq!(location.message, message);
        }
        assert!(source_location("--- FAIL: TestFoo (0.1s)", &dir).is_none());
        assert!(source_location("foo.go:0: bad", &dir).is_none());
        let absolute = dir.join("test.go");
        let text = format!("{}:5:7: message\n", absolute.display());
        assert_eq!(
            resolve_locations(&text, Path::new("/different/package")),
            text
        );
    }

    #[tokio::test]
    async fn capture_drains_but_bounds_output() {
        let data = vec![b'x'; OUTPUT_LIMIT + 123];
        let mut saved = Vec::new();
        assert!(capture(data.as_slice(), &mut saved).await.unwrap());
        assert_eq!(saved.len(), OUTPUT_LIMIT);
    }

    #[tokio::test]
    async fn startup_failure_is_a_retained_result() {
        let dir = tempfile::tempdir().unwrap();
        let (_tx, rx) = watch::channel(false);
        let result = run(
            &dir.path().join("missing-go"),
            &selection(dir.path(), &entry("TestFoo", None), None),
            rx,
            RUN_TIMEOUT,
        )
        .await;
        assert!(!result.success);
        assert!(result.status.contains("COULD NOT START"));
        assert!(!result.output.is_empty());
    }

    fn go_fixture() -> tempfile::TempDir {
        let dir = tempfile::Builder::new()
            .prefix("helix Go tests ")
            .tempdir()
            .unwrap();
        std::fs::write(
            dir.path().join("go.mod"),
            "module example.com/helix-runner\n\ngo 1.20\n",
        )
        .unwrap();
        // Deliberately run in a subpackage, not the module root.
        let package = dir.path().join("nested");
        std::fs::create_dir(&package).unwrap();
        std::fs::write(
            package.join("sample_test.go"),
            r#"package sample
import ("testing"; "time"; "os"; "fmt")
func TestPick(t *testing.T) {
    t.Run("case [one]+", func(t *testing.T) { t.Log("chosen case") })
    t.Run("other", func(t *testing.T) { t.Fatal("WRONG SUBTEST") })
}
func TestPickSibling(t *testing.T) { t.Fatal("WRONG TEST") }
func TestFail(t *testing.T) { t.Fatal("expected 42, got 0") }
func TestSkip(t *testing.T) { t.Skip("not available") }
func TestSlow(t *testing.T) {
    os.WriteFile("pid", []byte(fmt.Sprint(os.Getpid())), 0600)
    time.Sleep(60*time.Second)
}
"#,
        )
        .unwrap();
        dir
    }

    async fn run_go(dir: &Path, entry: &GoTestEntry) -> RunResult {
        let (_tx, rx) = watch::channel(false);
        run(
            Path::new("go"),
            &selection(dir, entry, None),
            rx,
            RUN_TIMEOUT,
        )
        .await
    }

    #[tokio::test]
    #[ignore = "requires Go on PATH; runs a temporary Go package"]
    async fn real_go_selection_failures_skips_and_build_errors() {
        let fixture = go_fixture();
        let dir = fixture.path().join("nested");
        let entries = find_go_tests_in_dir(&dir);
        let selected = entries
            .iter()
            .find(|e| e.subtest.as_deref() == Some("case [one]+"))
            .unwrap();
        let result = run_go(&dir, selected).await;
        assert!(result.success, "{}\n{}", result.status, result.output);
        assert!(result.output.contains("chosen case"));
        assert!(!result.output.contains("WRONG"));
        let parent = run_go(&dir, &entry("TestPick", None)).await;
        assert!(!parent.success && parent.output.contains("WRONG SUBTEST"));
        assert!(!parent.output.contains("WRONG TEST"));
        let missing = run_go(&dir, &entry("TestPick", Some("missing"))).await;
        assert!(missing.status.starts_with("NOT RUN"), "{}", missing.status);
        let failed = run_go(&dir, &entry("TestFail", None)).await;
        assert!(failed.status.starts_with("FAILED"));
        assert!(failed.output.contains("expected 42, got 0"));
        assert!(failed
            .output
            .lines()
            .filter_map(|l| source_location(l, &dir))
            .any(|l| l.path == dir.join("sample_test.go")));
        let skipped = run_go(&dir, &entry("TestSkip", None)).await;
        assert!(skipped.status.starts_with("SKIPPED"), "{}", skipped.status);
        std::fs::write(
            dir.join("broken.go"),
            "package sample\nvar broken = undefinedName\n",
        )
        .unwrap();
        let build = run_go(&dir, &entry("TestFail", None)).await;
        assert!(build.status.starts_with("FAILED"));
        assert!(build.output.contains("undefinedName"), "{}", build.output);
        assert!(build
            .output
            .lines()
            .filter_map(|l| source_location(l, &dir))
            .any(|l| l.path == dir.join("broken.go") && l.line == 1));
    }

    #[tokio::test]
    #[cfg(unix)]
    #[ignore = "requires Go on PATH; checks cancellation of a real test child"]
    async fn real_go_cancel_and_timeout_stop_test_children() {
        let fixture = go_fixture();
        let dir = fixture.path().join("nested");
        for cancelled in [true, false] {
            let (tx, rx) = watch::channel(false);
            let task_dir = dir.clone();
            let task = tokio::spawn(async move {
                run(
                    Path::new("go"),
                    &selection(&task_dir, &entry("TestSlow", None), None),
                    rx,
                    if cancelled {
                        RUN_TIMEOUT
                    } else {
                        Duration::from_secs(5)
                    },
                )
                .await
            });
            let pid_path = dir.join("pid");
            tokio::time::timeout(Duration::from_secs(30), async {
                while !pid_path.exists() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            let pid: i32 = std::fs::read_to_string(&pid_path).unwrap().parse().unwrap();
            if cancelled {
                tx.send(true).unwrap();
            }
            let result = tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .unwrap()
                .unwrap();
            assert!(result
                .status
                .starts_with(if cancelled { "CANCELLED" } else { "TIMED OUT" }));
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    // A killed process may remain a zombie briefly until reaped.
                    let alive = unsafe { libc::kill(pid, 0) } == 0;
                    let zombie = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                        .is_ok_and(|s| s.contains(") Z "));
                    if !alive || zombie {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            std::fs::remove_file(pid_path).unwrap();
        }
    }
}

#[cfg(all(test, feature = "integration"))]
mod editor_tests {
    use super::*;
    use crate::{application::Application, args::Args, config::Config};

    #[tokio::test(flavor = "multi_thread")]
    async fn saved_buffers_retained_hidden_results_and_navigation() {
        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("sample.go");
        std::fs::write(fixture.path().join("go.mod"), "module example.com/test\n").unwrap();
        std::fs::write(&path, "package sample\n\nvar Value = 42\n").unwrap();
        let mut args = Args::default();
        args.files
            .insert(path.clone(), vec![helix_core::Position::new(0, 0)]);
        let mut config = Config::default();
        config.editor.lsp.enable = false;
        let syntax = helix_core::syntax::Loader::new(
            helix_loader::config::default_lang_config()
                .try_into()
                .unwrap(),
        )
        .unwrap();
        let mut app = Application::new(args, config, syntax).unwrap();
        let editor = &mut app.editor;
        let origin = view!(editor).id;
        let source = doc!(editor).id();
        assert!(check_saved(editor, fixture.path()).is_ok());
        let id = result_buffer(editor);
        replace_output(editor, id, "RUNNING\n".into());
        assert!(!editor.documents[&id].is_modified());
        assert!(editor.documents[&id].readonly);
        let result_view = view!(editor).id;
        editor.focus(origin);
        editor.close(result_view);
        // Completion must update hidden results without changing source focus.
        replace_output(
            editor,
            id,
            format!("FAILED\n{}:3:7: expected 0, got 42\n", path.display()),
        );
        assert_eq!(doc!(editor).id(), source);
        assert_eq!(view!(editor).id, origin);
        assert!(editor.documents[&id]
            .text()
            .to_string()
            .starts_with("FAILED"));
        assert!(!editor.documents[&id].is_modified());
        assert!(check_saved(editor, fixture.path()).is_ok());
        focus_results(editor, id);
        assert_eq!(doc!(editor).id(), id);
        let location = source_location(
            &format!("{}:3:7: expected 0, got 42", path.display()),
            fixture.path(),
        )
        .unwrap();
        jump_to_location(editor, &location, Action::Replace);
        let (view, doc) = current_ref!(editor);
        assert_eq!(doc.id(), source);
        assert_eq!(
            doc.selection(view.id)
                .primary()
                .cursor_line(doc.text().slice(..)),
            2
        );
        // Editing a package buffer after discovery must prevent launching.
        let (view, doc) = current!(editor);
        let transaction = Transaction::change(
            doc.text(),
            std::iter::once((0, 0, Some("// unsaved\n".into()))),
        );
        doc.apply(&transaction, view.id);
        assert!(check_saved(editor, fixture.path()).is_err());
        // Reusing a result buffer clears old failure text before a new run.
        assert_eq!(result_buffer(editor), id);
        replace_output(editor, id, "RUNNING NEXT TEST\n".into());
        assert!(!editor.documents[&id]
            .text()
            .to_string()
            .contains("expected 0"));
        assert!(editor.close_document(id, true).is_ok());
        replace_output(editor, id, "finished after buffer was closed".into());
        assert!(!editor.documents.contains_key(&id));
        assert!(app.close().await.is_empty());
    }

    async fn keys(app: &mut Application, input: &str) {
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

    fn rerun_fixture() -> tempfile::TempDir {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::write(
            fixture.path().join("go.work"),
            "go 1.20\nuse (\n./first\n./sibling\n)\n",
        )
        .unwrap();
        for package in ["first", "sibling"] {
            std::fs::create_dir(fixture.path().join(package)).unwrap();
            std::fs::write(
                fixture.path().join(package).join("go.mod"),
                format!("module example.com/{package}\ngo 1.20\n"),
            )
            .unwrap();
        }
        std::fs::write(fixture.path().join("sibling/other_test.go"), "package sample\nimport \"testing\"\nfunc TestDifferent(t *testing.T) { t.Fatal(\"WRONG PACKAGE\") }\n").unwrap();
        std::fs::write(
            fixture.path().join("first/sample_test.go"),
            r#"package sample
import ("testing"; "os"; "time")
func TestPick(t *testing.T) {
    t.Run("chosen [case]+", func(t *testing.T) {
        f, err := os.OpenFile("runs", os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0600)
        if err != nil { t.Fatal(err) }
        f.WriteString("x"); f.Close()
        for { if _, err := os.Stat("hold"); err != nil { break }; time.Sleep(10*time.Millisecond) }
        t.Log("CHOSEN CASE")
    })
    t.Run("other", func(t *testing.T) { t.Fatal("UNSELECTED CASE") })
}
func TestPickSibling(t *testing.T) { t.Fatal("UNSELECTED TEST") }
"#,
        )
        .unwrap();
        fixture
    }

    fn test_app(path: &Path) -> Application {
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

    async fn finished_output(app: &mut Application) -> String {
        tokio::time::timeout(Duration::from_secs(30), async {
            while app.editor.go_test_cancel.is_some() {
                keys(app, "").await;
            }
        })
        .await
        .unwrap();
        let id = app.editor.go_test_doc_id.unwrap();
        app.editor.documents[&id].text().to_string()
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires Go; checks real picker reruns, package isolation and blocked starts"]
    async fn rerun_picker_selection_survives_file_changes_and_result_buffer_closure() {
        let fixture = rerun_fixture();
        let path = fixture.path().join("first/sample_test.go");
        let dir = path.parent().unwrap();
        let mut app = test_app(&path);
        keys(&mut app, ":go-test-last<ret>").await;
        assert!(app.editor.go_test_doc_id.is_none());
        assert!(app
            .editor
            .get_status()
            .unwrap()
            .0
            .contains("No Go test has been started"));
        keys(&mut app, "<space>tt").await;
        // The parent is first, followed by the chosen literal subtest.
        keys(&mut app, "<down><ret>").await;
        let original = finished_output(&mut app).await;
        assert!(
            original.starts_with("Go test: TestPick/chosen_[case]+\n"),
            "{original}"
        );
        assert!(original.contains("PASSED"), "{original}");
        assert!(!original.contains("UNSELECTED"), "{original}");
        assert_eq!(std::fs::read_to_string(dir.join("runs")).unwrap(), "x");

        // Opening and cancelling a new picker must not replace its last choice.
        keys(&mut app, ":go-test<ret>").await;
        keys(&mut app, "<esc>").await;
        let other = fixture.path().join("sibling/other_test.go");
        app.editor.open(&other, Action::Replace).unwrap();
        std::fs::write(dir.join("hold"), "").unwrap();
        keys(&mut app, "<space>tl").await;
        // A competing cursor command cannot overwrite the remembered target.
        keys(&mut app, ":go-test-nearest<ret>").await;
        assert!(app
            .editor
            .get_status()
            .unwrap()
            .0
            .contains("already running"));
        keys(&mut app, ":go-test-last<ret>").await;
        assert!(app
            .editor
            .get_status()
            .unwrap()
            .0
            .contains("already running"));
        std::fs::remove_file(dir.join("hold")).unwrap();
        let repeated = finished_output(&mut app).await;
        assert!(
            repeated.starts_with("Go test: TestPick/chosen_[case]+\n"),
            "{repeated}"
        );
        assert!(repeated.contains("PASSED"), "{repeated}");
        assert!(!repeated.contains("UNSELECTED") && !repeated.contains("WRONG PACKAGE"));
        assert_eq!(std::fs::read_to_string(dir.join("runs")).unwrap(), "xx");

        keys(&mut app, "<space>tr:go-test-last<ret>").await;
        assert!(finished_output(&mut app).await.contains("PASSED"));
        assert_eq!(std::fs::read_to_string(dir.join("runs")).unwrap(), "xxx");
        let result = app.editor.go_test_doc_id.unwrap();
        assert!(app.editor.close_document(result, true).is_ok());
        keys(&mut app, "<space>tl").await;
        assert!(finished_output(&mut app).await.contains("PASSED"));
        assert_eq!(std::fs::read_to_string(dir.join("runs")).unwrap(), "xxxx");
        assert!(app.close().await.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires Go and Go grammar; checks failed cursor reruns and original workspace guards"]
    async fn rerun_failed_cursor_parent_checks_saved_workspace_and_missing_targets() {
        let fixture = rerun_fixture();
        let path = fixture.path().join("first/sample_test.go");
        let original_source = std::fs::read_to_string(&path).unwrap();
        let dir = path.parent().unwrap();
        let mut app = test_app(&path);
        keys(&mut app, "3G<space>tn").await;
        let original = finished_output(&mut app).await;
        assert!(original.starts_with("Go test: TestPick\n"), "{original}");
        assert!(
            original.contains("FAILED") && original.contains("UNSELECTED CASE"),
            "{original}"
        );
        assert!(original.contains("running all of TestPick"));

        let sibling = fixture.path().join("sibling/other_test.go");
        let dirty = app.editor.open(&sibling, Action::Replace).unwrap();
        keys(&mut app, "3Gi// unsaved<esc><space>tn").await;
        assert!(app.editor.go_test_cancel.is_none());
        assert!(app
            .editor
            .get_status()
            .unwrap()
            .0
            .contains("Save modified files"));
        // A completely unrelated active workspace must not determine the guard.
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("other.go");
        std::fs::write(&outside_file, "package outside\n").unwrap();
        app.editor.open(&outside_file, Action::Replace).unwrap();
        keys(&mut app, "<space>tl").await;
        assert!(app.editor.go_test_cancel.is_none());
        assert!(app
            .editor
            .get_status()
            .unwrap()
            .0
            .contains("Save modified files"));
        assert_eq!(std::fs::read_to_string(dir.join("runs")).unwrap(), "x");
        assert!(app.editor.close_document(dirty, true).is_ok());
        keys(&mut app, "<space>tl").await;
        let repeated = finished_output(&mut app).await;
        assert!(repeated.starts_with("Go test: TestPick\n"), "{repeated}");
        assert!(repeated.contains("FAILED") && repeated.contains("UNSELECTED CASE"));
        assert!(!repeated.contains("UNSELECTED TEST"));
        assert!(repeated.contains("running all of TestPick"));
        assert_eq!(std::fs::read_to_string(dir.join("runs")).unwrap(), "xx");

        // A different valid cursor selection whose process cannot start must
        // leave the original failing parent available for rerun.
        app.editor.open(&sibling, Action::Replace).unwrap();
        let missing_package = fixture.path().join("moved-sibling");
        std::fs::rename(sibling.parent().unwrap(), &missing_package).unwrap();
        keys(&mut app, "3G<space>tn").await;
        assert!(finished_output(&mut app).await.contains("COULD NOT START"));
        std::fs::rename(&missing_package, sibling.parent().unwrap()).unwrap();
        keys(&mut app, "<space>tl").await;
        let restored = finished_output(&mut app).await;
        assert!(
            restored.starts_with("Go test: TestPick\n") && restored.contains("FAILED"),
            "{restored}"
        );
        assert_eq!(std::fs::read_to_string(dir.join("runs")).unwrap(), "xxx");

        // The remembered filter is not replaced by a new scan of the file.
        std::fs::write(&path, "package sample\nimport \"testing\"\nfunc TestReplacement(t *testing.T) { t.Fatal(\"WRONG TEST\") }\n").unwrap();
        keys(&mut app, ":go-test-last<ret>").await;
        assert!(finished_output(&mut app).await.contains("NOT RUN"));
        let moved = fixture.path().join("temporarily-moved");
        std::fs::rename(dir, &moved).unwrap();
        keys(&mut app, "<space>tl").await;
        assert!(finished_output(&mut app).await.contains("COULD NOT START"));
        std::fs::rename(&moved, dir).unwrap();
        std::fs::write(&path, original_source).unwrap();
        keys(&mut app, "<space>tl").await;
        assert!(finished_output(&mut app).await.contains("FAILED"));
        assert_eq!(std::fs::read_to_string(dir.join("runs")).unwrap(), "xxxx");
        assert!(app.close().await.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires Go on PATH and the Go grammar; runs tests at the primary cursor"]
    async fn cursor_command_runs_only_selected_test_and_checks_unsaved_files() {
        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("sample_test.go");
        std::fs::write(
            fixture.path().join("go.mod"),
            "module example.com/nearest\ngo 1.20\n",
        )
        .unwrap();
        let source = r#"package sample
import "testing"
// Multibyte text before the cursor: åäö
func TestPick(t *testing.T) {
    t.Run("chosen [case]+", func(t *testing.T) { t.Log("SELECTED CASE") })
    t.Run("other", func(t *testing.T) { t.Fatal("UNSELECTED CASE") })
}
func TestPickSibling(t *testing.T) { t.Fatal("UNSELECTED TEST") }
"#;
        std::fs::write(&path, source).unwrap();
        let mut args = Args::default();
        args.files
            .insert(path.clone(), vec![helix_core::Position::new(0, 0)]);
        let mut config = Config::default();
        config.editor.lsp.enable = false;
        let syntax = helix_core::syntax::Loader::new(
            helix_loader::config::default_lang_config()
                .try_into()
                .unwrap(),
        )
        .unwrap();
        let mut app = Application::new(args, config, syntax).unwrap();
        keys(&mut app, ":go-test-nearest<ret>").await;
        assert!(app.editor.go_test_doc_id.is_none());
        assert!(app
            .editor
            .get_status()
            .unwrap()
            .0
            .contains("not inside a Go test"));

        for (needle, command, expected_name, expected_status) in [
            (
                "t.Log(\"SELECTED CASE\")",
                "<space>tn",
                "TestPick/chosen [case]+",
                "PASSED",
            ),
            (
                "func TestPick(",
                ":go-test-nearest<ret>",
                "TestPick",
                "FAILED",
            ),
        ] {
            let (view, doc) = current!(app.editor);
            let position = doc.text().byte_to_char(source.find(needle).unwrap());
            doc.set_selection(view.id, Selection::point(position));
            keys(&mut app, command).await;
            let result_id = app.editor.go_test_doc_id.unwrap();
            tokio::time::timeout(Duration::from_secs(30), async {
                while app.editor.go_test_cancel.is_some() {
                    keys(&mut app, "").await;
                }
            })
            .await
            .unwrap();
            let output = app.editor.documents[&result_id].text().to_string();
            // Displayed names normalize spaces just like Go's test events.
            assert!(
                output.starts_with(&format!("Go test: {}\n", expected_name.replace(' ', "_"))),
                "{output}"
            );
            assert!(output.contains(expected_status), "{output}");
            assert!(!output.contains("UNSELECTED TEST"), "{output}");
            if expected_status == "PASSED" {
                assert!(!output.contains("UNSELECTED CASE"), "{output}");
            } else {
                assert!(output.contains("UNSELECTED CASE"), "{output}");
                assert!(output.contains("running all of TestPick"), "{output}");
            }
            assert_eq!(doc!(app.editor).path(), Some(&path));
            assert!(app.editor.debug_adapters.get_active_client().is_none());
            let (view, doc) = current!(app.editor);
            let position = doc
                .text()
                .byte_to_char(source.find("func TestPickSibling").unwrap());
            doc.set_selection(view.id, Selection::point(position));
            keys(&mut app, "<space>tl").await;
            let repeated = finished_output(&mut app).await;
            assert!(
                repeated.starts_with(&format!("Go test: {}\n", expected_name.replace(' ', "_"))),
                "{repeated}"
            );
            assert!(repeated.contains(expected_status), "{repeated}");
            assert!(!repeated.contains("UNSELECTED TEST"), "{repeated}");
        }
        keys(&mut app, "i// unsaved<esc><space>tn").await;
        assert!(app.editor.go_test_cancel.is_none());
        assert!(app
            .editor
            .get_status()
            .unwrap()
            .0
            .contains("Save modified files"));
        assert!(app.close().await.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires Go on PATH; exercises picker, job and source navigation"]
    async fn go_test_command_to_failure_location() {
        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("sample_test.go");
        std::fs::write(
            fixture.path().join("go.mod"),
            "module example.com/test\n\ngo 1.20\n",
        )
        .unwrap();
        std::fs::write(&path, "package sample\nimport \"testing\"\nfunc TestFailure(t *testing.T) {\n t.Fatal(\"expected 42, got 0\")\n}\n").unwrap();
        let mut args = Args::default();
        args.files
            .insert(path.clone(), vec![helix_core::Position::new(0, 0)]);
        let mut config = Config::default();
        config.editor.lsp.enable = false;
        let syntax = helix_core::syntax::Loader::new(
            helix_loader::config::default_lang_config()
                .try_into()
                .unwrap(),
        )
        .unwrap();
        let mut app = Application::new(args, config, syntax).unwrap();
        let source = doc!(app.editor).id();
        keys(&mut app, ":go-test<ret>").await;
        keys(&mut app, "<ret>").await;
        assert_eq!(doc!(app.editor).id(), source);
        let result_id = app.editor.go_test_doc_id.unwrap();
        tokio::time::timeout(Duration::from_secs(30), async {
            while app.editor.go_test_cancel.is_some() {
                keys(&mut app, "").await;
            }
        })
        .await
        .unwrap();
        assert!(app.editor.go_test_cancel.is_none());
        assert!(app.editor.documents[&result_id]
            .text()
            .to_string()
            .contains("FAILED"));
        assert!(app.editor.documents[&result_id]
            .text()
            .to_string()
            .contains("expected 42, got 0"));
        assert!(app.editor.debug_adapters.get_active_client().is_none());
        keys(&mut app, "<space>tr").await;
        assert_eq!(doc!(app.editor).id(), result_id);
        keys(&mut app, "<space>tf").await;
        keys(&mut app, "<ret>").await;
        let (view, doc) = current_ref!(app.editor);
        assert_eq!(doc.id(), source);
        assert_eq!(
            doc.selection(view.id)
                .primary()
                .cursor_line(doc.text().slice(..)),
            3
        );
        keys(&mut app, "i// unsaved<esc><space>tt").await;
        assert!(app.editor.go_test_cancel.is_none());
        assert!(app
            .editor
            .get_status()
            .unwrap()
            .0
            .contains("Save modified files"));
        assert!(app.close().await.is_empty());
    }
}
