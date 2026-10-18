//! Python tests through unittest or pytest, run from the root of the project
//! by the interpreter of its virtual environment. Tests are found by parsing
//! the sources; results are read from the runner's verbose report.
use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};

use helix_core::regex::Regex;
use helix_view::{
    editor::{Action, PythonTestRun, PythonTestRunner as Runner, TestRun},
    DocumentId, Editor,
};
use once_cell::sync::Lazy;
use serde_json::{json, Value};
use tokio::{
    process::Command,
    sync::{oneshot, watch},
};

use super::{
    check_idle, check_workspace_saved, execute, follow_launched, jump_to_location, pick_location,
    start, start_in, Debugger, Execution, Leftovers, RunResult, SourceLocation, Started, Streams,
    TRUNCATED,
};
use crate::{
    compositor,
    job::Callback,
    ui::{overlay::overlaid, Picker, PickerColumn},
};

mod corresponding;
mod discovery;

pub(in crate::commands) use corresponding::corresponding_tests;
use discovery::{relative, PythonTest};

pub(super) const BUFFER_NAME: &str = "[python-test]";
/// The first line of the output buffer starts with this.
pub(super) const TITLE: &str = "Python test: ";
/// The whole run; neither runner limits a single test.
pub(super) const RUN_TIMEOUT: Duration = Duration::from_secs(600);

/// The root of the project a directory belongs to, which the tests run in:
/// the nearest directory with the configuration of a project or of its
/// tests, else the repository.
fn root_of(dir: &Path) -> Option<PathBuf> {
    let is_project = |dir: &&Path| {
        discovery::PROJECT_FILES
            .iter()
            .any(|file| dir.join(file).is_file())
    };
    dir.ancestors()
        .find(is_project)
        .or_else(|| dir.ancestors().find(|dir| dir.join(".git").exists()))
        .map(Path::to_owned)
}

/// Whether the project's configuration says that pytest runs its tests.
fn configured_for_pytest(root: &Path) -> bool {
    let mentions = |file: &str, section: &str| {
        std::fs::read_to_string(root.join(file)).is_ok_and(|text| text.contains(section))
    };
    root.join("pytest.ini").is_file()
        || root.join(".pytest.ini").is_file()
        || mentions("pyproject.toml", "[tool.pytest")
        || mentions("setup.cfg", "[tool:pytest]")
        || mentions("tox.ini", "[pytest]")
}

/// A test module written for pytest: it imports pytest, or declares a test
/// that only pytest runs, a function of the module or a class without a base.
fn written_for_pytest(file: &Path) -> bool {
    // Cheap to check, and wrong only towards parsing a module.
    static MAY_BE_PYTEST_ONLY: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"(?m)^(?:(?:async[ \t]+)?def[ \t]+test|class[ \t]+Test)").unwrap()
    });
    let Ok(source) = std::fs::read_to_string(file) else {
        return false;
    };
    discovery::USES_PYTEST.is_match(&source)
        || MAY_BE_PYTEST_ONLY.is_match(&source)
            && discovery::grammar().is_ok_and(|grammar| {
                discovery::parse(grammar, &source, file)
                    .iter()
                    .any(|declared| !declared.test.unittest)
            })
}

/// The project of a directory and what runs its tests.
#[derive(Clone)]
struct Project {
    root: PathBuf,
    /// Whether the tests below the root are the project's. A directory that
    /// belongs to no project or repository is only its own files: it may be
    /// the home directory.
    below: bool,
    runner: Runner,
}

impl Project {
    /// Where the tests of a directory run, found without reading the tree.
    fn root(dir: &Path) -> (PathBuf, bool) {
        match root_of(dir) {
            Some(root) => (root, true),
            None => (dir.to_owned(), false),
        }
    }

    /// pytest when the project is set up for it, or when any of its test
    /// modules is written for it; unittest otherwise. pytest runs the tests
    /// written for unittest as well. This reads the project's test modules:
    /// not on the editor's thread.
    fn of(dir: &Path) -> Self {
        let (root, below) = Self::root(dir);
        let files = discovery::project_files(&root, below);
        let pytest = files.conftest
            || configured_for_pytest(&root)
            || files.tests.iter().any(|file| written_for_pytest(file));
        Self {
            runner: if pytest {
                Runner::Pytest
            } else {
                Runner::Unittest
            },
            root,
            below,
        }
    }

    /// Of the tests found, those that the runner runs.
    fn runs(&self, test: &PythonTest) -> bool {
        self.runner == Runner::Pytest || test.unittest
    }
}

/// The saved Python file in the focused view, once nothing in its project
/// that the tests read is unsaved.
fn saved_file(editor: &Editor) -> anyhow::Result<PathBuf> {
    let path = doc!(editor)
        .path()
        .filter(|path| path.extension().is_some_and(|e| e == "py"))
        .ok_or_else(|| anyhow::anyhow!("Open a saved Python file to run its tests"))?
        .to_owned();
    let (root, _) = Project::root(path.parent().unwrap_or(Path::new(".")));
    check_workspace_saved(editor, &root)?;
    Ok(path)
}

/// Finds what runs the project's tests, which reads them, off the editor's
/// thread, and then starts what `select` makes of it.
fn start_found(
    cx: &mut compositor::Context,
    dir: PathBuf,
    debugger: Option<Debugger>,
    select: impl FnOnce(&Project) -> anyhow::Result<PythonTestRun> + Send + 'static,
) {
    cx.jobs.callback(async move {
        let found = tokio::task::spawn_blocking(move || select(&Project::of(&dir))).await?;
        Ok(Callback::Editor(Box::new(move |editor| match found {
            Ok(target) => start_in(editor, TestRun::Python(target), debugger),
            Err(err) => editor.set_error(err.to_string()),
        })))
    });
}

/// The tests below the focused file's directory, as the runner finds them
/// there. With a debugger, they run under it.
pub(super) fn package(cx: &mut compositor::Context, debugger: Option<Debugger>) {
    match saved_file(cx.editor) {
        Ok(path) => {
            let dir = path.parent().unwrap().to_owned();
            start_found(cx, dir.clone(), debugger, move |project| {
                Ok(directory_selection(project, &dir))
            });
        }
        Err(err) => cx.editor.set_error(err.to_string()),
    }
}

/// Every test of the project, to choose one from. With a debugger, the
/// chosen test runs under it.
pub(super) fn pick(cx: &mut compositor::Context, debugger: Option<Debugger>) {
    let path = match check_idle(cx.editor).and_then(|()| saved_file(cx.editor)) {
        Ok(path) => path,
        Err(err) => {
            cx.editor.set_error(err.to_string());
            return;
        }
    };
    let dir = path.parent().unwrap().to_owned();
    cx.jobs.callback(async move {
        // A large project is many files to read and parse.
        let (project, tests) = tokio::task::spawn_blocking(move || {
            let project = Project::of(&dir);
            let tests = discovery::project_tests(&project.root, project.below);
            (project, tests)
        })
        .await?;
        Ok(Callback::EditorCompositor(Box::new(
            move |editor, compositor| {
                let tests: Vec<_> = match tests {
                    Ok(tests) => tests.into_iter().filter(|t| project.runs(t)).collect(),
                    Err(err) => {
                        editor.set_error(err.to_string());
                        return;
                    }
                };
                if tests.is_empty() {
                    editor.set_error(format!(
                        "No tests found in the test modules of {}",
                        project.root.display()
                    ));
                    return;
                }
                let root = project.root.clone();
                let picker = Picker::new(
                    [
                        PickerColumn::new("test", |t: &PythonTest, _: &PathBuf| t.name().into()),
                        PickerColumn::new("file", |t: &PythonTest, root: &PathBuf| {
                            relative(&t.file, root).into()
                        }),
                    ],
                    0,
                    tests,
                    root,
                    move |cx, test, _| {
                        let target = TestRun::Python(selection(&project, test, None));
                        start(cx, target, debugger.clone())
                    },
                )
                .with_preview(|_, test| {
                    Some((test.file.as_path().into(), Some((test.line, test.line))))
                });
                compositor.push(Box::new(overlaid(picker)));
            },
        )))
    });
}

pub(super) fn nearest(cx: &mut compositor::Context) {
    let found = (|| -> anyhow::Result<_> {
        check_idle(cx.editor)?;
        let path = saved_file(cx.editor)?;
        let (view, doc) = current_ref!(cx.editor);
        let source = doc.text().to_string();
        let byte = doc.text().char_to_byte(
            doc.selection(view.id)
                .primary()
                .cursor(doc.text().slice(..)),
        );
        let declared = discovery::parse(discovery::grammar()?, &source, &path);
        Ok((path, source, byte, declared))
    })();
    let (path, source, byte, declared) = match found {
        Ok(found) => found,
        Err(err) => {
            cx.editor.set_error(err.to_string());
            return;
        }
    };
    let dir = path.parent().unwrap().to_owned();
    start_found(cx, dir, None, move |project| {
        let declared: Vec<_> = declared
            .into_iter()
            .filter(|declared| project.runs(&declared.test))
            .collect();
        let (test, note) = at_cursor(&declared, &source, byte)?;
        Ok(selection(project, test, note))
    });
}

/// The test function around the cursor, else the test class around it. A
/// class is run with a note, because more runs than the cursor is on.
fn at_cursor<'a>(
    declared: &'a [discovery::Declared],
    source: &str,
    byte: usize,
) -> anyhow::Result<(&'a PythonTest, Option<String>)> {
    // Leading indentation belongs to the declaration on this line. Do not
    // cross a newline and choose the next test.
    let byte = byte
        + source[byte.min(source.len())..]
            .bytes()
            .take_while(|b| matches!(b, b' ' | b'\t'))
            .count();
    // A nested declaration comes after the one it is in. A declaration ends
    // before the line break of its last line, where the cursor can be too.
    let innermost = |function: bool| {
        declared
            .iter()
            .rev()
            .find(|d| {
                d.test.function.is_some() == function
                    && (d.bytes.start..=d.bytes.end).contains(&byte)
            })
            .map(|d| &d.test)
    };
    if let Some(test) = innermost(true) {
        return Ok((test, None));
    }
    let class = innermost(false)
        .ok_or_else(|| anyhow::anyhow!("Cursor is not inside a test function or a test class"))?;
    let note = format!(
        "No test function at the cursor; running all tests of {}.",
        class.name()
    );
    Ok((class, Some(note)))
}

/// A test, or a class of them, by the name its runner knows it by.
fn selection(project: &Project, test: &PythonTest, note: Option<String>) -> PythonTestRun {
    let (name, import_paths) = match project.runner {
        Runner::Unittest => {
            let (top, module) = discovery::module_of(&test.file).unwrap_or_default();
            let name = format!("{module}.{}", test.name());
            // `python -m unittest` imports from the directory it runs in.
            let paths = (top != project.root).then_some(top).into_iter().collect();
            (name, paths)
        }
        Runner::Pytest => (test.node_id(&project.root), Vec::new()),
    };
    PythonTestRun {
        root: project.root.clone(),
        directory: test.file.parent().unwrap_or(&project.root).to_owned(),
        runner: project.runner,
        args: vec!["-v".into(), name.clone()],
        import_paths,
        name,
        selection_note: note,
    }
}

/// All tests below a directory, as the runner finds them there.
fn directory_selection(project: &Project, dir: &Path) -> PythonTestRun {
    let shown = relative(dir, &project.root);
    let args = match project.runner {
        Runner::Unittest => {
            let mut args = vec!["discover".into(), "-v".into(), "-s".into(), shown.clone()];
            // The tests of a package import as its modules.
            let top = discovery::import_root(dir);
            if top != dir {
                args.extend(["-t".into(), relative(&top, &project.root)]);
            }
            args
        }
        Runner::Pytest => vec!["-v".into(), shown.clone()],
    };
    PythonTestRun {
        root: project.root.clone(),
        directory: dir.to_owned(),
        runner: project.runner,
        args,
        import_paths: Vec::new(),
        name: format!("All tests in {}", Path::new(&shown).join("").display()),
        selection_note: None,
    }
}

/// The interpreter of a project: the active virtual environment, else the
/// nearest `.venv` at or above `dir`, else `python3` on the PATH.
fn interpreter(dir: &Path) -> PathBuf {
    interpreter_of(
        dir,
        std::env::var_os("VIRTUAL_ENV").as_deref().map(Path::new),
    )
}

/// A virtual environment whose interpreter is gone still counts, so that the
/// run says so.
fn interpreter_of(dir: &Path, active: Option<&Path>) -> PathBuf {
    let python = |venv: &Path| {
        [venv.join("bin/python"), venv.join("Scripts/python.exe")]
            .into_iter()
            .find(|python| python.symlink_metadata().is_ok())
    };
    active
        .and_then(python)
        .or_else(|| dir.ancestors().find_map(|dir| python(&dir.join(".venv"))))
        .unwrap_or_else(|| PathBuf::from(if cfg!(windows) { "python" } else { "python3" }))
}

fn module(runner: Runner) -> &'static str {
    match runner {
        Runner::Unittest => "unittest",
        Runner::Pytest => "pytest",
    }
}

/// PYTHONPATH with the directories the selected test modules import from in front.
fn python_path(target: &PythonTestRun) -> Option<OsString> {
    if target.import_paths.is_empty() {
        return None;
    }
    let inherited = std::env::var_os("PYTHONPATH");
    let inherited = inherited.iter().flat_map(std::env::split_paths);
    std::env::join_paths(target.import_paths.iter().cloned().chain(inherited)).ok()
}

/// An argument as a shell would need it written.
fn quoted(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_alphanumeric() || "_-./:=@%+,[]".contains(c));
    if plain {
        arg.to_owned()
    } else {
        format!("{arg:?}")
    }
}

/// The command as it runs in the project's root.
fn command_line(target: &PythonTestRun) -> String {
    let path = match target.import_paths.as_slice() {
        [] => String::new(),
        paths => {
            let paths: Vec<_> = paths.iter().map(|p| relative(p, &target.root)).collect();
            format!("PYTHONPATH={} ", quoted(&paths.join(":")))
        }
    };
    let python = interpreter(&target.directory);
    let args: Vec<_> = target.args.iter().map(|arg| quoted(arg)).collect();
    format!(
        "{path}{} -m {} {}",
        quoted(&relative(&python, &target.root)),
        module(target.runner),
        args.join(" ")
    )
}

/// The lines above the result in the output buffer.
pub(super) fn header(target: &PythonTestRun, debug: bool) -> String {
    let notice = target
        .selection_note
        .as_ref()
        .map(|note| format!("Original selection: {note}\n"))
        .unwrap_or_default();
    let debugger = if debug {
        "The debugger starts the tests; the results follow when they are done.\n"
    } else {
        ""
    };
    format!(
        "{TITLE}{}\nDirectory: {}\nCommand: {}\n\
         Tests read saved files from disk. Save and rerun after edits.\n{debugger}\
         Space t f: go to this line's test or source | Space t r: results | Space t c: cancel\n{notice}\n",
        target.name,
        target.root.display(),
        command_line(target),
    )
}

/// What a debug launch of the run sets in the arguments of its template:
/// debugpy runs the runner's module in the project's root, through the
/// editor's terminal so that the run gets the output. The template's own
/// interpreter is kept; without one, the run's is used.
pub(super) fn debug_args(target: &PythonTestRun, args: &mut serde_json::Map<String, Value>) {
    args.remove("program");
    args.insert("module".into(), module(target.runner).into());
    args.insert("args".into(), json!(target.args));
    args.insert("cwd".into(), json!(target.root.to_string_lossy()));
    args.insert("console".into(), "integratedTerminal".into());
    args.insert("redirectOutput".into(), false.into());
    args.entry("python")
        .or_insert_with(|| json!(interpreter(&target.directory).to_string_lossy()));
    let env = args
        .entry("env")
        .or_insert_with(|| json!({}))
        .as_object_mut();
    if let Some(env) = env {
        env.insert("PYTHONUNBUFFERED".into(), "1".into());
        if let Some(path) = python_path(target) {
            env.insert("PYTHONPATH".into(), json!(path.to_string_lossy()));
        }
        // The results are read from plain text: without the editor's colours.
        for colours in ["FORCE_COLOR", "PY_COLORS", "PYTHON_COLORS"] {
            env.insert(colours.into(), Value::Null);
        }
    }
}

/// A run under the debugger, whose adapter starts the program.
pub(super) async fn run_launched(
    target: &PythonTestRun,
    started: oneshot::Receiver<Started>,
    cancel: watch::Receiver<bool>,
    deadline: Duration,
) -> RunResult {
    // Tests that start servers or other processes do not always stop them.
    match follow_launched(started, cancel, deadline, Leftovers::Killed, |_| {}).await {
        Ok(execution) => outcome(target, execution),
        Err(status) => RunResult {
            started: false,
            status,
            output: "The debug adapter's messages are in the debug log (:debug-log).\n".into(),
            success: false,
        },
    }
}

pub(super) async fn run(
    target: &PythonTestRun,
    cancel: watch::Receiver<bool>,
    deadline: Duration,
) -> RunResult {
    let python = interpreter(&target.directory);
    let mut command = Command::new(&python);
    command
        .arg("-m")
        .arg(module(target.runner))
        .args(&target.args)
        .current_dir(&target.root)
        // unittest reports on the error stream and the tests print to the
        // other: unbuffered, they stay in the order they were written.
        .env("PYTHONUNBUFFERED", "1")
        // The results are read from plain text.
        .env_remove("FORCE_COLOR")
        .env_remove("PY_COLORS")
        .env_remove("PYTHON_COLORS");
    if let Some(path) = python_path(target) {
        command.env("PYTHONPATH", path);
    }
    // Tests that start servers or other processes do not always stop them.
    match execute(
        command,
        cancel,
        deadline,
        Streams::Merged,
        Leftovers::Killed,
        |_| {},
    )
    .await
    {
        Ok(execution) => outcome(target, execution),
        Err(err) => RunResult {
            started: false,
            status: format!("COULD NOT START {}", python.display()),
            output: format!("{err}\n"),
            success: false,
        },
    }
}

/// The status of a finished run, read from the report at its end.
fn outcome(target: &PythonTestRun, execution: Execution) -> RunResult {
    let mut output = String::from_utf8_lossy(&execution.stdout).into_owned();
    let (status, success) = match execution.exit {
        Err(reason) => (reason, false),
        Ok(exit) => match target.runner {
            Runner::Unittest => unittest_status(target, exit.code(), &output),
            Runner::Pytest => pytest_status(target, exit.code(), &output),
        },
    };
    if execution.truncated {
        output.push_str(TRUNCATED);
    }
    RunResult {
        started: true,
        status,
        output,
        success,
    }
}

/// Whether the run is of all tests below a directory, not of a selected one.
fn is_directory_run(target: &PythonTestRun) -> bool {
    match target.runner {
        Runner::Unittest => target.args.first().is_some_and(|arg| arg == "discover"),
        Runner::Pytest => target
            .args
            .last()
            .is_some_and(|arg| !arg.contains("::") && !arg.ends_with(".py")),
    }
}

/// Where a selection that ran nothing was looked for.
fn not_observed(target: &PythonTestRun) -> String {
    if is_directory_run(target) {
        format!(
            "NOT RUN: no tests were found in {}",
            relative(&target.directory, &target.root)
        )
    } else {
        "NOT RUN: the selected test was not observed (renamed, removed, or excluded?)".into()
    }
}

/// unittest's report ends in the number of tests that ran, and then in what
/// became of them:
///
/// ```text
/// ----------------------------------------------------------------------
/// Ran 7 tests in 0.002s
///
/// FAILED (failures=2, errors=1, skipped=1, expected failures=1)
/// ```
///
/// Only the whole block counts: a test can print a line that looks like part of it.
static UNITTEST_SUMMARY: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?m)^-{70}\r?\nRan ([0-9]+) tests? in [0-9.]+s\r?\n\r?\n(OK|FAILED|NO TESTS RAN)(?: \(([^)\n]*)\))?\r?$",
    )
    .unwrap()
});
/// A selected name that unittest could not find, which it reports as the
/// error of a test of its own.
static UNITTEST_MISSING: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?m)^ERROR: \S+ \(unittest\.loader\._FailedTest\.\S+\)\r?\n-+\r?\nAttributeError: ",
    )
    .unwrap()
});

fn unittest_status(target: &PythonTestRun, code: Option<i32>, output: &str) -> (String, bool) {
    let Some(summary) = UNITTEST_SUMMARY.captures_iter(output).last() else {
        return (exited(code), false);
    };
    let ran: usize = summary[1].parse().unwrap_or(0);
    let counts: HashMap<&str, usize> = summary
        .get(3)
        .map_or("", |counts| counts.as_str())
        .split(", ")
        .filter_map(|count| {
            let (what, number) = count.split_once('=')?;
            Some((what, number.parse().ok()?))
        })
        .collect();
    let count = |what: &str| counts.get(what).copied().unwrap_or(0);
    let numbers = |numbers: &[(&str, &str, &str)]| {
        let parts: Vec<_> = numbers
            .iter()
            .filter_map(|(key, one, many)| match count(key) {
                0 => None,
                1 => Some(format!("1 {one}")),
                n => Some(format!("{n} {many}")),
            })
            .collect();
        parts.join(", ")
    };
    const SKIPPED: [(&str, &str, &str); 2] = [
        ("skipped", "skipped", "skipped"),
        ("expected failures", "expected failure", "expected failures"),
    ];
    if UNITTEST_MISSING.is_match(output) && ran == count("errors") {
        return (not_observed(target), false);
    }
    // A fixture of a class or module that fails or skips leaves no test run,
    // and is counted all the same.
    match &summary[2] {
        "NO TESTS RAN" => (not_observed(target), false),
        "OK" if ran == 0 && count("skipped") == 0 => (not_observed(target), false),
        "FAILED" => {
            let failed = numbers(&[
                ("failures", "failure", "failures"),
                ("errors", "error", "errors"),
                (
                    "unexpected successes",
                    "unexpected success",
                    "unexpected successes",
                ),
                SKIPPED[0],
                SKIPPED[1],
            ]);
            (format!("FAILED ({failed}; {ran} ran)"), false)
        }
        // Without failures every test that was not skipped passed: subtests
        // only add failures.
        _ => {
            let passed = ran.saturating_sub(count("skipped") + count("expected failures"));
            let skipped = numbers(&SKIPPED);
            let rest = if skipped.is_empty() {
                String::new()
            } else {
                format!(", {skipped}")
            };
            if passed == 0 {
                (format!("SKIPPED ({skipped})"), false)
            } else {
                (format!("PASSED ({passed} passed{rest})"), true)
            }
        }
    }
}

/// pytest's last line counts what became of the tests:
/// `===== 3 failed, 3 passed, 1 skipped, 1 error in 0.03s =====`.
static PYTEST_SUMMARY: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?m)^=+ (.+?) in [0-9.]+s(?: \([^)]*\))? =+\r?$").unwrap());

fn pytest_status(target: &PythonTestRun, code: Option<i32>, output: &str) -> (String, bool) {
    let summary = PYTEST_SUMMARY
        .captures_iter(output)
        .last()
        .map(|summary| summary[1].to_owned());
    if summary.is_none() && output.contains("No module named pytest") {
        let python = interpreter(&target.directory);
        return (
            format!("NOT RUN: pytest is not installed for {}", python.display()),
            false,
        );
    }
    let count = |what: &str| {
        summary.as_deref().map_or(0, |summary| {
            summary
                .split(", ")
                .filter_map(|count| count.split_once(' '))
                .filter(|(_, kind)| *kind == what)
                .filter_map(|(number, _)| number.parse::<usize>().ok())
                .sum()
        })
    };
    match code {
        // Every test passed or was skipped.
        Some(0) => match summary {
            Some(summary) if count("passed") == 0 && count("xpassed") == 0 => {
                (format!("SKIPPED ({summary})"), false)
            }
            Some(summary) => (format!("PASSED ({summary})"), true),
            None => ("PASSED".into(), true),
        },
        Some(4)
            if output.contains("ERROR: not found:")
                || output.contains("ERROR: file or directory not found:") =>
        {
            (not_observed(target), false)
        }
        Some(5) => (not_observed(target), false),
        _ => match summary {
            Some(summary) => (format!("FAILED ({summary})"), false),
            None => (exited(code), false),
        },
    }
}

fn exited(code: Option<i32>) -> String {
    match code {
        Some(code) => format!("FAILED (exit code {code})"),
        None => "FAILED (stopped by a signal)".into(),
    }
}

/// A frame of a traceback, `  File "/src/test_calc.py", line 12, in test_add`,
/// also as pytest quotes it in a failure, after `E`.
static FRAME: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"^(?:E\s+)?\s*File "([^"]+)", line ([0-9]+)(?:, in (.+?))?\s*$"#).unwrap()
});
/// pytest's place of a failure or of a frame, `tests/test_calc.py:10: AssertionError`,
/// and of a warning in its summary, indented.
static PYTEST_LOCATION: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^\s*([^\s:][^:]*\.py):([0-9]+): (.*?)\s*$").unwrap());

/// The location an output line names. pytest names files relative to the
/// directory it runs in; tracebacks name them as they were imported.
fn source_location(line: &str, root: &Path) -> Option<SourceLocation> {
    let (path, number, message) = if let Some(frame) = FRAME.captures(line) {
        let function = frame.get(3).map(|f| format!("in {}", f.as_str()));
        (frame.get(1)?.as_str(), frame.get(2)?.as_str(), function)
    } else {
        let location = PYTEST_LOCATION.captures(line)?;
        let message = location.get(3).map(|m| m.as_str().to_owned());
        (
            location.get(1)?.as_str(),
            location.get(2)?.as_str(),
            message,
        )
    };
    let line = number.parse::<usize>().ok()?.checked_sub(1)?;
    // `<frozen importlib._bootstrap>` and `<string>` are no files.
    if path.starts_with('<') {
        return None;
    }
    Some(SourceLocation {
        path: helix_stdx::path::canonicalize(root.join(path)),
        line,
        message: message.unwrap_or_default(),
    })
}

/// A test that unittest names, `test_add (test_calc.CalcTests.test_add)`: at
/// the start of its result, with a subtest's parameters after it, and in the
/// heading of its failure. A class or module fixture is named by its scope:
/// `setUpClass (test_calc.CalcTests)`.
static UNITTEST_TEST: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^\s*(?:(?:FAIL|ERROR|UNEXPECTED SUCCESS): )?([A-Za-z_][\w.]*) \(([A-Za-z_][\w.]*)\)(?: \[.*?\])?(?: \(.*\))?(?: \.\.\..*)?$",
    )
    .unwrap()
});
/// The line that starts the report of a failure, or a section of pytest's
/// report: what follows it belongs to none of the tests above it.
static SEPARATOR: Lazy<Regex> = Lazy::new(|| Regex::new(r"^={3,}").unwrap());
/// The heading of a failure that unittest reports with its traceback.
static UNITTEST_FAILURE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(?:FAIL|ERROR|UNEXPECTED SUCCESS): ").unwrap());
/// pytest's result of a test, `tests/test_calc.py::TestCalc::test_add PASSED`,
/// and the line of the summary that repeats a failure, also of a subtest:
/// `SUBFAILED(value=1) tests/test_calc.py::CalcTests::test_table - …`.
static PYTEST_RESULT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^(?:(?:SUB)?(FAILED|ERROR|XFAIL|XPASS|SKIPPED|PASSED)(?:\([^)]*\))? )?(\S+?\.py::\S+)(?:\s.*)?$",
    )
    .unwrap()
});
/// The heading of a failure that pytest reports, `____ TestCalc.test_method ____`,
/// with the parameters of a subtest after the name.
static PYTEST_HEADING: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^_{2,} (?:ERROR at (?:setup|teardown) of )?([A-Za-z_][\w.]*(?:\[.*\])?)(?: \(.*\))? _{2,}$",
    )
    .unwrap()
});
/// The lines of a report that belong to no single test.
static REPORT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^(?:={3,}.*|-{3,}|Ran [0-9]+ tests? in .*|OK(?: \(.*\))?|FAILED \(.*\)|NO TESTS RAN|platform .*|rootdir: .*|configfile: .*|cachedir: .*|plugins: .*|testpaths: .*|collect(?:ing|ed) .*)$",
    )
    .unwrap()
});

/// unittest's fixtures for a whole class or module, which it names by their scope.
const SCOPE_FIXTURES: [&str; 4] = [
    "setUpClass",
    "tearDownClass",
    "setUpModule",
    "tearDownModule",
];

/// A test as the output of a run names it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Reported {
    /// unittest's name: `test_calc.CalcTests.test_add`.
    Unittest(String),
    /// pytest's node id: `tests/test_calc.py::TestCalc::test_add[1-2]`.
    Node(String),
    /// The name pytest heads a failure with: `TestCalc.test_add[1-2]`.
    Heading(String),
}

impl Reported {
    fn name(&self) -> &str {
        match self {
            Self::Unittest(name) | Self::Node(name) | Self::Heading(name) => name,
        }
    }
}

/// The test that an output line names.
fn reported(line: &str) -> Option<Reported> {
    if let Some(test) = UNITTEST_TEST.captures(line) {
        let (name, scope) = (&test[1], &test[2]);
        // Python 3.11 and later name the method in the parentheses too.
        let id = if scope.ends_with(&format!(".{name}")) || SCOPE_FIXTURES.contains(&name) {
            scope.to_owned()
        } else {
            format!("{scope}.{name}")
        };
        return Some(Reported::Unittest(id));
    }
    if let Some(result) = PYTEST_RESULT.captures(line) {
        return Some(Reported::Node(result[2].to_owned()));
    }
    PYTEST_HEADING
        .captures(line)
        .map(|heading| Reported::Heading(heading[1].to_owned()))
}

/// The test an output line belongs to: named on the line, else by the
/// closest result or heading above it.
fn test_on_line(lines: &[String], line: usize) -> Option<Reported> {
    if REPORT.is_match(&lines[line]) {
        return None;
    }
    lines[..=line]
        .iter()
        .rev()
        .take_while(|line| !SEPARATOR.is_match(line))
        .find_map(|line| reported(line))
}

fn root_dir(lines: &[String]) -> Option<PathBuf> {
    lines
        .iter()
        .take_while(|line| !line.is_empty())
        .find_map(|line| line.strip_prefix("Directory: "))
        .map(PathBuf::from)
}

/// The test modules of the project that a run was in, read when needed.
struct Index {
    /// The directory the run was in, which pytest's node ids are relative to.
    root: PathBuf,
    files: Option<Vec<PathBuf>>,
    parsed: HashMap<PathBuf, Vec<PythonTest>>,
}

impl Index {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
            files: None,
            parsed: HashMap::new(),
        }
    }

    fn files(&mut self) -> &[PathBuf] {
        let root = &self.root;
        self.files.get_or_insert_with(|| {
            let (_, below) = Project::root(root);
            discovery::test_files(root, below)
        })
    }

    /// The test modules imported by a name.
    fn modules(&mut self, name: &str) -> Vec<PathBuf> {
        self.files()
            .iter()
            .filter(|file| discovery::module_of(file).is_some_and(|(_, module)| module == name))
            .cloned()
            .collect()
    }

    fn tests(&mut self, file: &Path) -> &[PythonTest] {
        self.parsed.entry(file.to_owned()).or_insert_with(|| {
            discovery::grammar()
                .map(|grammar| discovery::file_tests(grammar, file))
                .unwrap_or_default()
        })
    }

    /// The declaration of the test called `names` in `file`; the file itself
    /// when no names are left.
    fn declared(&mut self, file: &Path, names: &[&str]) -> Vec<SourceLocation> {
        let message = names.join(".");
        if names.is_empty() {
            return vec![SourceLocation {
                path: file.to_owned(),
                line: 0,
                message,
            }];
        }
        self.tests(file)
            .iter()
            .filter(|test| test.name() == message)
            .map(|test| SourceLocation {
                path: test.file.clone(),
                line: test.line,
                message: message.clone(),
            })
            .collect()
    }

    /// Where the test of a reported name is declared. Parameters of pytest
    /// and the names of subtests are not in the sources.
    fn declarations(&mut self, reported: &Reported) -> Vec<SourceLocation> {
        let unparameterized = |name: &str| name.split('[').next().unwrap_or(name).to_owned();
        match reported {
            Reported::Unittest(id) => {
                // A module that failed to import, or a missing name.
                let id = id
                    .strip_prefix("unittest.loader._FailedTest.")
                    .unwrap_or(id);
                let parts: Vec<_> = id.split('.').collect();
                for split in (1..=parts.len()).rev() {
                    let files = self.modules(&parts[..split].join("."));
                    let found: Vec<_> = files
                        .iter()
                        .flat_map(|file| self.declared(file, &parts[split..]))
                        .collect();
                    if !found.is_empty() {
                        return found;
                    }
                }
                Vec::new()
            }
            Reported::Node(id) => {
                let id = unparameterized(id);
                let mut parts = id.split("::");
                let file = helix_stdx::path::canonicalize(self.root.join(parts.next().unwrap()));
                let names: Vec<_> = parts.collect();
                if file.is_file() {
                    self.declared(&file, &names)
                } else {
                    Vec::new()
                }
            }
            Reported::Heading(name) => {
                let name = unparameterized(name);
                let names: Vec<_> = name.split('.').collect();
                self.files()
                    .to_vec()
                    .iter()
                    .flat_map(|file| self.declared(file, &names))
                    .collect()
            }
        }
    }
}

/// The Python reading of [`super::show_locations`].
pub(super) fn show_locations(cx: &mut compositor::Context, id: DocumentId) {
    let lines: Vec<String> = cx.editor.documents[&id]
        .text()
        .lines()
        .map(|line| line.to_string().trim_end().to_owned())
        .collect();
    let Some(root) = root_dir(&lines) else {
        cx.editor
            .set_error("The output does not say which directory the tests ran in");
        return;
    };
    let mut index = Index::new(&root);
    let (view, doc) = current_ref!(cx.editor);
    if doc.id() == id {
        let line = doc
            .selection(view.id)
            .primary()
            .cursor_line(doc.text().slice(..));
        if let Some(location) =
            source_location(&lines[line], &root).filter(|location| location.path.is_file())
        {
            jump_to_location(cx.editor, &location, Action::Replace);
            return;
        }
        if let Some(test) = test_on_line(&lines, line) {
            match <[_; 1]>::try_from(index.declarations(&test)) {
                Ok([location]) => jump_to_location(cx.editor, &location, Action::Replace),
                Err(locations) if locations.is_empty() => cx.editor.set_error(format!(
                    "{} is not declared in the project's test modules under that name",
                    test.name()
                )),
                Err(locations) => pick_location(cx, locations),
            }
            return;
        }
    }
    let locations = reported_locations(&lines, &root, &mut index);
    if locations.is_empty() {
        cx.editor.set_error(
            "No source locations or failed tests reported; Space t r shows the full output",
        );
        return;
    }
    pick_location(cx, locations);
}

/// Whether a file is part of an interpreter's standard library or of an
/// installed package: below `lib/python3.12` or a `site-packages`.
fn installed(path: &Path) -> bool {
    let parts: Vec<_> = path
        .components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect();
    parts.windows(2).any(|pair| {
        let version = pair[1].strip_prefix("python");
        pair[0] == "lib"
            && version.is_some_and(|v| v.chars().all(|c| c.is_ascii_digit() || c == '.'))
    }) || parts
        .iter()
        .any(|part| part == "site-packages" || part == "dist-packages")
}

/// Every line of the tested code that the output names, in the order named,
/// and where each failed test is declared. Frames of the interpreter's
/// standard library and of installed packages are left out.
fn reported_locations(lines: &[String], root: &Path, index: &mut Index) -> Vec<SourceLocation> {
    let mut locations = Vec::new();
    let mut seen = HashSet::new();
    for line in lines {
        let found = if let Some(location) = source_location(line, root) {
            if location.path.is_file() && !installed(&location.path) {
                vec![location]
            } else {
                Vec::new()
            }
        } else if UNITTEST_FAILURE.is_match(line)
            || PYTEST_RESULT.captures(line).is_some_and(|result| {
                matches!(result.get(1).map(|m| m.as_str()), Some("FAILED" | "ERROR"))
            })
        {
            let message = line.to_owned();
            reported(line)
                .map(|test| index.declarations(&test))
                .unwrap_or_default()
                .into_iter()
                .map(|location| SourceLocation {
                    message: message.clone(),
                    ..location
                })
                .collect()
        } else {
            Vec::new()
        };
        for location in found {
            // A failure is named in its heading and in the summary as well.
            if seen.insert((location.path.clone(), location.line)) {
                locations.push(location);
            }
        }
    }
    locations
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (file, contents) in files {
            let path = dir.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        dir
    }

    fn root(dir: &tempfile::TempDir) -> PathBuf {
        helix_stdx::path::canonicalize(dir.path())
    }

    #[test]
    fn projects_are_found_upwards_and_say_what_runs_them() {
        let dir = tree(&[
            (".git/HEAD", ""),
            (
                "tools/test_audit.py",
                "import unittest\nclass A(unittest.TestCase):\n    def test_a(self): pass\n",
            ),
            ("app/pyproject.toml", "[project]\nname = \"app\"\n"),
            ("app/tests/test_app.py", "import unittest\n"),
            ("lib/setup.cfg", "[tool:pytest]\n"),
            ("lib/tests/test_lib.py", ""),
            ("conf/pyproject.toml", "[project]\n"),
            ("conf/tests/conftest.py", ""),
            ("conf/tests/test_conf.py", ""),
            ("free/pyproject.toml", "[project]\n"),
            ("free/test_free.py", "def test_free():\n    pass\n"),
            ("bare/pyproject.toml", "[project]\n"),
            (
                "bare/test_bare.py",
                "class TestBare:\n    def test_bare(self): pass\n",
            ),
            ("helper/pyproject.toml", "[project]\n"),
            (
                "helper/test_helper.py",
                "class TestingHelpers:\n    def check(self): pass\n",
            ),
        ]);
        let root = root(&dir);
        let project = |dir: &str| {
            let project = Project::of(&root.join(dir));
            (project.root, project.runner)
        };
        assert_eq!(project("tools"), (root.clone(), Runner::Unittest));
        assert_eq!(project("app/tests"), (root.join("app"), Runner::Unittest));
        assert_eq!(project("lib/tests"), (root.join("lib"), Runner::Pytest));
        assert_eq!(project("conf/tests"), (root.join("conf"), Runner::Pytest));
        // A conftest.py anywhere in the project counts.
        assert_eq!(project("conf"), (root.join("conf"), Runner::Pytest));
        assert_eq!(project("free"), (root.join("free"), Runner::Pytest));
        assert_eq!(project("bare"), (root.join("bare"), Runner::Pytest));
        // A class named Test… without tests says nothing.
        assert_eq!(project("helper"), (root.join("helper"), Runner::Unittest));
        let pyproject = tree(&[
            (
                "pyproject.toml",
                "[tool.pytest.ini_options]\ntestpaths = [\"tests\"]\n",
            ),
            ("tests/test_x.py", ""),
        ]);
        assert_eq!(
            Project::of(&pyproject.path().join("tests")).runner,
            Runner::Pytest
        );
    }

    #[test]
    fn the_interpreter_is_the_nearest_virtual_environment() {
        let dir = tree(&[
            ("project/.venv/bin/python", ""),
            ("project/tools/sidecar/.venv/bin/python", ""),
            ("active/bin/python", ""),
        ]);
        let root = root(&dir);
        assert_eq!(
            interpreter_of(&root.join("project/tools"), None),
            root.join("project/.venv/bin/python")
        );
        assert_eq!(
            interpreter_of(&root.join("project/tools/sidecar"), None),
            root.join("project/tools/sidecar/.venv/bin/python")
        );
        assert_eq!(
            interpreter_of(&root.join("project/tools"), Some(&root.join("active"))),
            root.join("active/bin/python")
        );
        // An active environment without an interpreter is no environment.
        assert_eq!(
            interpreter_of(&root.join("project"), Some(&root.join("missing"))),
            root.join("project/.venv/bin/python")
        );
        assert_eq!(interpreter_of(&root, None), PathBuf::from("python3"));
    }

    fn test(file: &Path, classes: &[&str], function: Option<&str>) -> PythonTest {
        PythonTest {
            file: file.to_owned(),
            classes: classes.iter().map(|class| (*class).to_owned()).collect(),
            function: function.map(str::to_owned),
            line: 0,
            unittest: true,
        }
    }

    #[test]
    fn selections_name_tests_as_their_runner_does() {
        let dir = tree(&[
            ("pyproject.toml", ""),
            ("tests/test_calc.py", ""),
            ("src/shop/__init__.py", ""),
            ("src/shop/tests/__init__.py", ""),
            ("src/shop/tests/test_cart.py", ""),
            ("test_root.py", ""),
        ]);
        let root = root(&dir);
        let unittest = Project {
            root: root.clone(),
            below: true,
            runner: Runner::Unittest,
        };
        let pytest = Project {
            root: root.clone(),
            below: true,
            runner: Runner::Pytest,
        };
        let method = test(
            &root.join("tests/test_calc.py"),
            &["CalcTests"],
            Some("test_add"),
        );
        let run = selection(&unittest, &method, None);
        assert_eq!(run.name, "test_calc.CalcTests.test_add");
        assert_eq!(run.args, ["-v", "test_calc.CalcTests.test_add"]);
        assert_eq!(run.import_paths, [root.join("tests")]);
        assert_eq!(run.directory, root.join("tests"));
        assert_eq!(
            command_line(&run),
            "PYTHONPATH=tests python3 -m unittest -v test_calc.CalcTests.test_add"
        );
        let cart = test(
            &root.join("src/shop/tests/test_cart.py"),
            &["CartTests"],
            None,
        );
        let run = selection(&unittest, &cart, Some("a note".into()));
        assert_eq!(run.args, ["-v", "shop.tests.test_cart.CartTests"]);
        assert_eq!(run.import_paths, [root.join("src")]);
        let top = test(&root.join("test_root.py"), &[], Some("test_top"));
        assert!(selection(&unittest, &top, None).import_paths.is_empty());
        let run = selection(&pytest, &method, None);
        assert_eq!(run.name, "tests/test_calc.py::CalcTests::test_add");
        assert_eq!(run.args, ["-v", "tests/test_calc.py::CalcTests::test_add"]);
        assert!(run.import_paths.is_empty());
        assert_eq!(
            command_line(&run),
            "python3 -m pytest -v tests/test_calc.py::CalcTests::test_add"
        );

        // A directory runs as the runner finds its tests there.
        let run = directory_selection(&unittest, &root.join("tests"));
        assert_eq!(run.args, ["discover", "-v", "-s", "tests"]);
        assert_eq!(run.name, "All tests in tests/");
        let run = directory_selection(&unittest, &root.join("src/shop/tests"));
        assert_eq!(
            run.args,
            ["discover", "-v", "-s", "src/shop/tests", "-t", "src"]
        );
        let run = directory_selection(&unittest, &root);
        assert_eq!(run.args, ["discover", "-v", "-s", "."]);
        assert_eq!(run.name, "All tests in ./");
        assert_eq!(
            directory_selection(&pytest, &root.join("tests")).args,
            ["-v", "tests"]
        );
    }

    #[test]
    fn the_header_shows_the_command_and_the_directory() {
        let dir = tree(&[(".venv/bin/python", ""), ("tests/test calc.py", "")]);
        let root = root(&dir);
        let project = Project {
            root: root.clone(),
            below: true,
            runner: Runner::Unittest,
        };
        let method = test(&root.join("tests/test calc.py"), &["A"], Some("test_a"));
        let run = selection(&project, &method, Some("a note".into()));
        let header = header(&run, false);
        assert!(header.starts_with(&format!(
            "Python test: test calc.A.test_a\nDirectory: {}\n",
            root.display()
        )));
        assert!(header.contains(
            "Command: PYTHONPATH=tests .venv/bin/python -m unittest -v \"test calc.A.test_a\"\n"
        ));
        assert!(header.ends_with("Original selection: a note\n\n"));
        assert_eq!(root_dir(&lines(&header)), Some(root.clone()));
        let debugged = super::header(&run, true);
        assert!(debugged.contains("The debugger starts the tests"));
        assert_eq!(root_dir(&lines(&debugged)), Some(root));
    }

    fn lines(output: &str) -> Vec<String> {
        output.lines().map(str::to_owned).collect()
    }

    /// What Python 3.14 prints for the tests of `test_calc.py`.
    const UNITTEST: &str = "\
Python test: All tests in tests/
Directory: /src
Command: python3 -m unittest discover -v -s tests

FAILED (2 failures, 1 error, 1 skipped, 1 expected failure; 7 ran)

test_add (test_calc.CalcTests.test_add) ... printed from test_add
ok
test_add_more (test_calc.CalcTests.test_add_more)
Adds more numbers. ... FAIL
test_error (test_calc.CalcTests.test_error) ... ERROR
test_skip (test_calc.CalcTests.test_skip) ... skipped 'not now'
test_sub (test_calc.CalcTests.test_sub) ...
  test_sub (test_calc.CalcTests.test_sub) (i=1) ... FAIL
test_xfail (test_calc.CalcTests.test_xfail) ... expected failure
testCamel (test_calc.Other.testCamel) ... to stderr
ok

======================================================================
ERROR: test_error (test_calc.CalcTests.test_error)
----------------------------------------------------------------------
Traceback (most recent call last):
  File \"/src/tests/test_calc.py\", line 15, in test_error
    raise RuntimeError(\"boom\")
RuntimeError: boom

======================================================================
FAIL: test_add_more (test_calc.CalcTests.test_add_more)
Adds more numbers.
----------------------------------------------------------------------
Traceback (most recent call last):
  File \"/src/tests/test_calc.py\", line 12, in test_add_more
    self.assertEqual(1 + 2, 4)
    ~~~~~~~~~~~~~~~~^^^^^^^^^^
AssertionError: 3 != 4

======================================================================
FAIL: test_sub (test_calc.CalcTests.test_sub) (i=1)
----------------------------------------------------------------------
Traceback (most recent call last):
  File \"/src/tests/test_calc.py\", line 28, in test_sub
    self.assertNotEqual(i, 1)
AssertionError: 1 == 1

----------------------------------------------------------------------
Ran 7 tests in 0.002s

FAILED (failures=2, errors=1, skipped=1, expected failures=1)
";

    fn unittest_run(args: &[&str]) -> PythonTestRun {
        PythonTestRun {
            root: "/src".into(),
            directory: "/src/tests".into(),
            runner: Runner::Unittest,
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            import_paths: Vec::new(),
            name: String::new(),
            selection_note: None,
        }
    }

    fn report(ran: usize, status: &str) -> String {
        format!(
            "{}\nRan {ran} tests in 0.001s\n\n{status}\n",
            "-".repeat(70)
        )
    }

    #[test]
    fn unittest_results_count_what_unittest_counts() {
        let directory = unittest_run(&["discover", "-v", "-s", "tests"]);
        let status = |code, output: &str| unittest_status(&directory, Some(code), output);
        assert_eq!(
            status(1, UNITTEST),
            (
                "FAILED (2 failures, 1 error, 1 skipped, 1 expected failure; 7 ran)".into(),
                false
            )
        );
        assert_eq!(
            status(0, &report(3, "OK")),
            ("PASSED (3 passed)".into(), true)
        );
        assert_eq!(
            status(0, &report(4, "OK (skipped=1, expected failures=1)")),
            (
                "PASSED (2 passed, 1 skipped, 1 expected failure)".into(),
                true
            )
        );
        assert_eq!(
            status(0, &report(2, "OK (skipped=2)")),
            ("SKIPPED (2 skipped)".into(), false)
        );
        assert_eq!(
            status(1, &report(1, "FAILED (unexpected successes=1)")),
            ("FAILED (1 unexpected success; 1 ran)".into(), false)
        );
        assert_eq!(
            status(5, &report(0, "NO TESTS RAN")),
            ("NOT RUN: no tests were found in tests".into(), false)
        );
        // A class fixture that fails or skips leaves nothing run.
        assert_eq!(
            status(1, &report(0, "FAILED (errors=1)")),
            ("FAILED (1 error; 0 ran)".into(), false)
        );
        assert_eq!(
            status(0, &report(0, "OK (skipped=1)")),
            ("SKIPPED (1 skipped)".into(), false)
        );
        // Only the whole block counts, and the last one.
        let printed = format!(
            "Ran 9 tests in 1s\n\nOK\n{}",
            report(1, "FAILED (errors=1)")
        );
        assert_eq!(status(1, &printed).0, "FAILED (1 error; 1 ran)");
        assert_eq!(
            status(1, "Traceback (most recent call last):\nSyntaxError: x\n"),
            ("FAILED (exit code 1)".into(), false)
        );

        // A selected test that is not there is an error of unittest's own.
        let selected = unittest_run(&["-v", "test_calc.CalcTests.nope"]);
        let missing = format!(
            "nope (unittest.loader._FailedTest.nope) ... ERROR\n\n{}\nERROR: nope (unittest.loader._FailedTest.nope)\n{}\nAttributeError: type object 'CalcTests' has no attribute 'nope'\n\n{}",
            "=".repeat(70),
            "-".repeat(70),
            report(1, "FAILED (errors=1)")
        );
        assert!(unittest_status(&selected, Some(1), &missing)
            .0
            .starts_with("NOT RUN: the selected test was not observed"));
        // Not when other tests ran with it.
        let with_others = missing.replace("Ran 1 tests", "Ran 2 tests");
        assert!(unittest_status(&selected, Some(1), &with_others)
            .0
            .starts_with("FAILED"));
    }

    fn pytest_run() -> PythonTestRun {
        PythonTestRun {
            runner: Runner::Pytest,
            ..unittest_run(&["-v", "tests/test_calc.py::test_add"])
        }
    }

    /// What pytest 9 prints for the tests of a `test_calc.py`.
    const PYTEST: &str = "\
Python test: All tests in tests/
Directory: /src
Command: python3 -m pytest -v tests

FAILED (3 failed, 3 passed, 1 skipped, 1 error)

============================= test session starts ==============================
platform linux -- Python 3.13.15, pytest-9.0.3, pluggy-1.6.0 -- /usr/bin/python3
cachedir: .pytest_cache
rootdir: /src
configfile: pyproject.toml
collecting ... collected 8 items

tests/test_calc.py::test_add PASSED                                      [ 12%]
tests/test_calc.py::test_fail FAILED                                     [ 25%]
tests/test_calc.py::test_param[1-2] PASSED                               [ 37%]
tests/test_calc.py::test_param[2-2] FAILED                               [ 50%]
tests/test_calc.py::TestCalc::test_method FAILED                         [ 62%]
tests/test_calc.py::TestCalc::test_skipped SKIPPED (not now)             [ 75%]
tests/test_calc.py::TestCalc::TestNested::test_inner PASSED              [ 87%]
tests/test_calc.py::test_fixture_error ERROR                             [100%]

==================================== ERRORS ====================================
_____________________ ERROR at setup of test_fixture_error _____________________

    @pytest.fixture
    def broken():
>       raise RuntimeError(\"fixture boom\")
E       RuntimeError: fixture boom

tests/test_calc.py:37: RuntimeError
=================================== FAILURES ===================================
__________________________________ test_fail ___________________________________

    def test_fail():
>       assert 1 + 2 == 4
E       assert (1 + 2) == 4

tests/test_calc.py:10: AssertionError
_______________________________ test_param[2-2] ________________________________

a = 2, b = 2

    @pytest.mark.parametrize(\"a,b\", [(1, 2), (2, 2)])
    def test_param(a, b):
>       assert a + 1 == b
E       assert (2 + 1) == 2

tests/test_calc.py:15: AssertionError
_____________________________ TestCalc.test_method _____________________________

self = <test_calc.TestCalc object at 0x762b464a5590>

    def test_method(self):
>       assert helper() == 3
E       assert 2 == 3
E        +  where 2 = helper()

tests/test_calc.py:20: AssertionError
=========================== short test summary info ============================
FAILED tests/test_calc.py::test_fail - assert (1 + 2) == 4
FAILED tests/test_calc.py::test_param[2-2] - assert (2 + 1) == 2
FAILED tests/test_calc.py::TestCalc::test_method - assert 2 == 3
ERROR tests/test_calc.py::test_fixture_error - RuntimeError: fixture boom
=============== 3 failed, 3 passed, 1 skipped, 1 error in 0.03s ================
";

    #[test]
    fn pytest_results_use_its_own_summary() {
        let run = pytest_run();
        let status = |code, output: &str| pytest_status(&run, Some(code), output);
        assert_eq!(
            status(1, PYTEST),
            (
                "FAILED (3 failed, 3 passed, 1 skipped, 1 error)".into(),
                false
            )
        );
        assert_eq!(
            status(
                0,
                "============ 2 passed, 1 warning in 0.01s ============\n"
            ),
            ("PASSED (2 passed, 1 warning)".into(), true)
        );
        assert_eq!(
            status(0, "=== 1 skipped, 1 xfailed in 0.01s ===\n"),
            ("SKIPPED (1 skipped, 1 xfailed)".into(), false)
        );
        assert!(status(5, "=== no tests ran in 0.01s ===\n")
            .0
            .starts_with("NOT RUN: the selected test was not observed"));
        let directory = PythonTestRun {
            args: vec!["-v".into(), "tests".into()],
            ..pytest_run()
        };
        assert_eq!(
            pytest_status(&directory, Some(5), "=== no tests ran in 0.01s ===\n").0,
            "NOT RUN: no tests were found in tests"
        );
        assert!(status(
            4,
            "ERROR: not found: /src/tests/test_calc.py::nope\n(no match in any of [<Module test_calc.py>])\n\n=== no tests ran in 0.01s ===\n"
        )
        .0
        .starts_with("NOT RUN"));
        assert_eq!(
            status(
                2,
                "=== 1 error in 0.08s ===\nInterrupted: 1 error during collection\n"
            )
            .0,
            "FAILED (1 error)"
        );
        assert!(status(1, "/usr/bin/python3: No module named pytest\n")
            .0
            .starts_with("NOT RUN: pytest is not installed for "));
    }

    #[test]
    fn locations_in_tracebacks_and_pytest_reports() {
        let root = Path::new("/src");
        for (text, file, line, message) in [
            (
                "  File \"/src/tests/test_calc.py\", line 15, in test_error",
                "/src/tests/test_calc.py",
                14,
                "in test_error",
            ),
            (
                "  File \"/src/a dir/x.py\", line 3",
                "/src/a dir/x.py",
                2,
                "",
            ),
            (
                "E     File \"/src/tests/test_syntax.py\", line 2",
                "/src/tests/test_syntax.py",
                1,
                "",
            ),
            (
                "tests/test_calc.py:10: AssertionError",
                "/src/tests/test_calc.py",
                9,
                "AssertionError",
            ),
            ("src/calc.py:3: in add", "/src/src/calc.py", 2, "in add"),
            (
                "/src/tests/test_calc.py:5: DeprecationWarning: old",
                "/src/tests/test_calc.py",
                4,
                "DeprecationWarning: old",
            ),
            (
                "  /src/tests/test_w.py:8: DeprecationWarning: old",
                "/src/tests/test_w.py",
                7,
                "DeprecationWarning: old",
            ),
        ] {
            let location = source_location(text, root).unwrap_or_else(|| panic!("{text}"));
            assert_eq!(location.path, Path::new(file), "{text}");
            assert_eq!(location.line, line, "{text}");
            assert_eq!(location.message, message, "{text}");
        }
        for text in [
            "  File \"<frozen importlib._bootstrap>\", line 1387, in _gcd_import",
            "  File \"<string>\", line 1, in <module>",
            "tests/test_calc.py::test_fail FAILED",
            "test_add (test_calc.CalcTests.test_add) ... ok",
            "  File \"/src/x.py\", line 0, in f",
            "Ran 7 tests in 0.002s",
        ] {
            assert!(source_location(text, root).is_none(), "{text}");
        }
    }

    #[test]
    fn output_lines_belong_to_the_test_named_above_them() {
        let unittest = |name: &str| Some(Reported::Unittest(name.to_owned()));
        let lines = lines(UNITTEST);
        let owner = |needle: &str| {
            let line = lines.iter().position(|l| l.contains(needle)).unwrap();
            test_on_line(&lines, line)
        };
        for outside in [
            "Python test:",
            "Command:",
            "FAILED (2 failures",
            "=======",
            "Ran 7 tests",
            "FAILED (failures=2",
        ] {
            assert_eq!(owner(outside), None, "{outside}");
        }
        assert_eq!(
            owner("printed from"),
            unittest("test_calc.CalcTests.test_add")
        );
        assert_eq!(
            owner("Adds more numbers. ..."),
            unittest("test_calc.CalcTests.test_add_more")
        );
        assert_eq!(
            owner("(i=1) ... FAIL"),
            unittest("test_calc.CalcTests.test_sub")
        );
        assert_eq!(owner("to stderr"), unittest("test_calc.Other.testCamel"));
        assert_eq!(
            owner("RuntimeError: boom"),
            unittest("test_calc.CalcTests.test_error")
        );
        assert_eq!(
            owner("AssertionError: 1 == 1"),
            unittest("test_calc.CalcTests.test_sub")
        );
        // Before Python 3.11 the parentheses held the class alone; fixtures
        // of a class or module are named by it.
        for (line, name) in [
            (
                "test_add (test_calc.CalcTests) ... ok",
                "test_calc.CalcTests.test_add",
            ),
            (
                "ERROR: setUpClass (test_setup.SetupFails)",
                "test_setup.SetupFails",
            ),
            ("ERROR: setUpModule (test_setup)", "test_setup"),
            (
                "ERROR: test_broken (unittest.loader._FailedTest.test_broken)",
                "unittest.loader._FailedTest.test_broken",
            ),
            ("UNEXPECTED SUCCESS: test_u (m.C.test_u)", "m.C.test_u"),
            // A module of a package that did not import, and a subtest with a message.
            (
                "ERROR: shop.tests.test_broken (unittest.loader._FailedTest.shop.tests.test_broken)",
                "unittest.loader._FailedTest.shop.tests.test_broken",
            ),
            ("FAIL: test_cases (m.BTests.test_cases) [bad] (n=1)", "m.BTests.test_cases"),
        ] {
            assert_eq!(reported(line), unittest(name), "{line}");
        }
        // What follows a separator belongs to none of the tests above it.
        let unknown = lines_of(&format!(
            "test_a (m.A.test_a) ... ok\n{}\nERROR: something unittest names otherwise\nTraceback (most recent call last):\n",
            "=".repeat(70)
        ));
        assert_eq!(test_on_line(&unknown, 3), None);

        let node = |id: &str| Some(Reported::Node(id.to_owned()));
        let lines = lines_of(PYTEST);
        let owner = |needle: &str| {
            let line = lines.iter().position(|l| l.contains(needle)).unwrap();
            test_on_line(&lines, line)
        };
        for outside in [
            "test session starts",
            "rootdir:",
            "collecting ...",
            "short test summary",
        ] {
            assert_eq!(owner(outside), None, "{outside}");
        }
        assert_eq!(
            owner("::test_param[1-2] PASSED"),
            node("tests/test_calc.py::test_param[1-2]")
        );
        assert_eq!(
            owner("TestNested::test_inner"),
            node("tests/test_calc.py::TestCalc::TestNested::test_inner")
        );
        assert_eq!(
            owner("fixture boom\")"),
            Some(Reported::Heading("test_fixture_error".into()))
        );
        assert_eq!(
            owner("where 2 = helper()"),
            Some(Reported::Heading("TestCalc.test_method".into()))
        );
        assert_eq!(
            owner("FAILED tests/test_calc.py::test_param[2-2]"),
            node("tests/test_calc.py::test_param[2-2]")
        );
        // pytest 9 reports the subtests of unittest on their own.
        for (line, expected) in [
            (
                "SUBFAILED(value=1) tests/test_calc.py::CalcTests::test_table - AssertionError...",
                node("tests/test_calc.py::CalcTests::test_table"),
            ),
            (
                "________________________ CalcTests.test_table (value=1) ________________________",
                Some(Reported::Heading("CalcTests.test_table".into())),
            ),
        ] {
            assert_eq!(reported(line), expected, "{line}");
        }
        assert!(PYTEST_RESULT
            .captures("SUBFAILED(value=1) tests/test_calc.py::CalcTests::test_table - x")
            .is_some_and(|result| &result[1] == "FAILED"));
    }

    fn lines_of(output: &str) -> Vec<String> {
        lines(output)
    }

    const CALC: &str = "import unittest


class CalcTests(unittest.TestCase):
    def test_add(self):
        pass

    def test_error(self):
        raise RuntimeError('boom')


def test_fail():
    assert False


class TestCalc:
    def test_method(self):
        pass
";

    #[test]
    fn reported_names_resolve_to_declarations() {
        let dir = tree(&[
            ("pyproject.toml", ""),
            ("tests/test_calc.py", CALC),
            ("tests/test_broken.py", "import missing\n"),
            ("src/shop/__init__.py", ""),
            ("src/shop/test_shop.py", "import unittest\n\nclass ShopTests(unittest.TestCase):\n    def test_buy(self):\n        pass\n"),
        ]);
        let root = root(&dir);
        let calc = root.join("tests/test_calc.py");
        let mut index = Index::new(&root);
        let mut found = |reported: Reported| -> Vec<(PathBuf, usize)> {
            index
                .declarations(&reported)
                .into_iter()
                .map(|location| (location.path, location.line))
                .collect()
        };
        let unittest = |name: &str| Reported::Unittest(name.to_owned());
        assert_eq!(
            found(unittest("test_calc.CalcTests.test_add")),
            [(calc.clone(), 4)]
        );
        assert_eq!(found(unittest("test_calc.CalcTests")), [(calc.clone(), 3)]);
        assert_eq!(
            found(unittest("shop.test_shop.ShopTests.test_buy")),
            [(root.join("src/shop/test_shop.py"), 3)]
        );
        // A module that did not import is its file.
        assert_eq!(
            found(unittest("unittest.loader._FailedTest.test_broken")),
            [(root.join("tests/test_broken.py"), 0)]
        );
        assert!(found(unittest("test_calc.CalcTests.test_gone")).is_empty());
        assert!(found(unittest("unittest.loader._FailedTest.nope")).is_empty());
        let node = |id: &str| Reported::Node(id.to_owned());
        assert_eq!(
            found(node("tests/test_calc.py::test_fail")),
            [(calc.clone(), 11)]
        );
        assert_eq!(
            found(node("tests/test_calc.py::test_fail[1-2]")),
            [(calc.clone(), 11)]
        );
        assert_eq!(
            found(node("tests/test_calc.py::TestCalc::test_method")),
            [(calc.clone(), 16)]
        );
        assert_eq!(found(node("tests/test_calc.py")), [(calc.clone(), 0)]);
        assert!(found(node("tests/test_gone.py::test_x")).is_empty());
        assert_eq!(
            found(Reported::Heading("TestCalc.test_method".into())),
            [(calc.clone(), 16)]
        );
        assert_eq!(
            found(Reported::Heading("test_fail[1-2]".into())),
            [(calc.clone(), 11)]
        );
    }

    #[test]
    fn the_standard_library_and_installed_packages_are_no_tested_code() {
        for path in [
            "/usr/lib/python3.12/unittest/case.py",
            "/nix/store/x-python3-3.12.12/lib/python3.12/json/__init__.py",
            "/home/u/.local/share/uv/python/cpython-3.14.6-linux-x86_64-gnu/lib/python3.14/os.py",
            "/src/.venv/lib/python3.12/site-packages/numpy/core.py",
            "/usr/lib/python3/dist-packages/yaml/__init__.py",
        ] {
            assert!(installed(Path::new(path)), "{path}");
        }
        for path in [
            "/src/tests/test_calc.py",
            "/src/lib/python_helpers/x.py",
            "/src/../shared/lib/calc.py",
        ] {
            assert!(!installed(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn failures_and_frames_of_the_project_are_listed_once() {
        let dir = tree(&[
            ("pyproject.toml", ""),
            ("tests/test_calc.py", CALC),
            (".venv/lib/python3.12/site-packages/lib.py", "\n\n\n"),
        ]);
        let root = root(&dir);
        let calc = root.join("tests/test_calc.py");
        let library = root.join(".venv/lib/python3.12/site-packages/lib.py");
        let output = format!(
            "{}\nERROR: test_error (test_calc.CalcTests.test_error)\n{}\nTraceback (most recent call last):\n  File \"{}\", line 9, in test_error\n  File \"{}\", line 2, in call\n  File \"/nowhere/gone.py\", line 3, in f\nRuntimeError: boom\n",
            "=".repeat(70),
            "-".repeat(70),
            calc.display(),
            library.display(),
        );
        let found: Vec<_> = reported_locations(&lines(&output), &root, &mut Index::new(&root))
            .into_iter()
            .map(|location| (location.path, location.line, location.message))
            .collect();
        assert_eq!(
            found,
            [
                (
                    calc.clone(),
                    7,
                    "ERROR: test_error (test_calc.CalcTests.test_error)".into()
                ),
                (calc.clone(), 8, "in test_error".into()),
            ]
        );
        let pytest = PYTEST.replace("/src", &root.to_string_lossy());
        let found: Vec<_> = reported_locations(&lines(&pytest), &root, &mut Index::new(&root))
            .into_iter()
            .map(|location| location.line)
            .collect();
        // The places of the failures, then the declarations of those the
        // summary repeats, where the places have not named them already.
        assert_eq!(found, [36, 9, 14, 19, 11, 16]);
    }

    #[test]
    fn the_test_at_the_cursor_else_its_class() {
        let source = "import unittest\n\nclass T(unittest.TestCase):\n    def test_a(self):\n        x = 1\n\n    def helper(self):\n        pass\n\ndef test_free():\n    pass\n\nclass Plain:\n    pass\n";
        let declared = discovery::parse(
            discovery::grammar().unwrap(),
            source,
            Path::new("test_t.py"),
        );
        let at = |needle: &str| {
            at_cursor(&declared, source, source.find(needle).unwrap())
                .map(|(test, note)| (test.name(), note.is_some()))
                .map_err(|err| err.to_string())
        };
        assert_eq!(at("x = 1"), Ok(("T.test_a".into(), false)));
        // At the end of its last line, where the line break is not its own.
        assert_eq!(at("\n\n    def helper"), Ok(("T.test_a".into(), false)));
        assert_eq!(at("\n\nclass Plain"), Ok(("test_free".into(), false)));
        // Indentation before a definition belongs to it.
        assert_eq!(at("    def test_a"), Ok(("T.test_a".into(), false)));
        assert_eq!(at("pass\n\ndef test_free"), Ok(("T".into(), true)));
        assert_eq!(at("def test_free"), Ok(("test_free".into(), false)));
        assert!(at("import unittest").is_err());
        assert!(at("class Plain").is_err());
    }

    #[test]
    fn a_debug_launch_runs_the_selection_through_the_editor() {
        let mut run = unittest_run(&["-v", "test_calc.CalcTests.test_add"]);
        run.import_paths = vec!["/src/tests".into()];
        let mut args = serde_json::Map::new();
        args.insert("program".into(), "{0}".into());
        args.insert("console".into(), "internalConsole".into());
        args.insert("justMyCode".into(), false.into());
        args.insert("env".into(), json!({ "DEBUG": "1" }));
        debug_args(&run, &mut args);
        assert_eq!(args.get("program"), None);
        assert_eq!(args["module"], "unittest");
        assert_eq!(args["args"], json!(["-v", "test_calc.CalcTests.test_add"]));
        assert_eq!(args["cwd"], "/src");
        assert_eq!(args["console"], "integratedTerminal");
        assert_eq!(args["redirectOutput"], false);
        assert_eq!(args["justMyCode"], false);
        assert_eq!(args["env"]["DEBUG"], "1");
        assert_eq!(args["env"]["PYTHONUNBUFFERED"], "1");
        assert_eq!(args["env"]["FORCE_COLOR"], Value::Null);
        assert!(args["env"]["PYTHONPATH"]
            .as_str()
            .unwrap()
            .starts_with("/src/tests"));
        // The run's interpreter, unless the template has one.
        assert_eq!(args["python"], json!(interpreter(&run.directory)));
        let mut args = serde_json::Map::new();
        args.insert("python".into(), "/opt/python".into());
        debug_args(&run, &mut args);
        assert_eq!(args["python"], "/opt/python");
    }

    #[tokio::test]
    async fn startup_failure_is_a_retained_result() {
        let dir = tree(&[(".venv/bin/python", "")]);
        let run = PythonTestRun {
            root: root(&dir),
            directory: root(&dir),
            ..unittest_run(&["-v", "test_x"])
        };
        let (_tx, rx) = watch::channel(false);
        let result = super::run(&run, rx, RUN_TIMEOUT).await;
        assert!(!result.success && !result.started);
        assert!(
            result.status.starts_with("COULD NOT START"),
            "{}",
            result.status
        );
        assert!(!result.output.is_empty());
    }
}

#[cfg(test)]
pub(super) mod real {
    use super::*;

    /// A project below a repository, in a path with spaces: flat tests in
    /// `tests/`, and a package with tests of its own in `src/`. `Slow` only
    /// waits while a file named `hold` exists in the root.
    pub(in crate::commands::test) fn python_fixture() -> tempfile::TempDir {
        let fixture = tempfile::Builder::new()
            .prefix("helix python tests ")
            .tempdir()
            .unwrap();
        let root = helix_stdx::path::canonicalize(fixture.path());
        for (file, contents) in [
            ("pyproject.toml", "[project]\nname = \"shop\"\nversion = \"0.1\"\n".to_owned()),
            (
                "tests/test_calc.py",
                r#"import unittest

from helpers import double


class CalcTests(unittest.TestCase):
    def test_add(self):
        print("CHOSEN TEST")
        self.assertEqual(double(2), 4)

    def test_add_more(self):
        print("SIBLING TEST")
        self.assertEqual(1 + 2, 4)

    @unittest.skip("not available")
    def test_skipped(self):
        pass

    def test_table(self):
        for value in range(3):
            with self.subTest(value=value):
                self.assertNotEqual(value, 1)


class OtherTests(unittest.TestCase):
    def test_other(self):
        print("OTHER TEST")
"#
                .to_owned(),
            ),
            ("tests/helpers.py", "def double(x):\n    return 2 * x\n".to_owned()),
            (
                "tests/test_slow.py",
                format!(
                    r#"import os
import time
import unittest


class SlowTests(unittest.TestCase):
    def test_slow(self):
        if not os.path.exists({hold:?}):
            return
        with open({pid:?}, "w") as pid:
            pid.write(str(os.getpid()))
        time.sleep(60)
"#,
                    hold = root.join("hold").to_string_lossy(),
                    pid = root.join("pid").to_string_lossy(),
                ),
            ),
            ("src/shop/__init__.py", String::new()),
            ("src/shop/cart.py", "def total(prices):\n    return sum(prices)\n".to_owned()),
            ("src/shop/tests/__init__.py", String::new()),
            (
                "src/shop/tests/test_cart.py",
                "import unittest\n\nfrom ..cart import total\n\n\nclass CartTests(unittest.TestCase):\n    def test_total(self):\n        self.assertEqual(total([1, 2]), 3)\n".to_owned(),
            ),
            ("empty/notes.py", String::new()),
        ] {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        std::fs::create_dir(root.join(".git")).unwrap();
        fixture
    }

    pub(in crate::commands::test) async fn run_python(target: &PythonTestRun) -> RunResult {
        let (_tx, rx) = watch::channel(false);
        run(target, rx, RUN_TIMEOUT).await
    }

    fn report(result: &RunResult) -> String {
        format!("{}\n{}", result.status, result.output)
    }

    #[tokio::test]
    #[ignore = "requires python3 on PATH and the Python grammar"]
    async fn real_unittest_selections_directories_and_failures() {
        let fixture = python_fixture();
        let root = helix_stdx::path::canonicalize(fixture.path());
        let project = Project::of(&root.join("tests"));
        assert_eq!(
            (project.root.as_path(), project.runner),
            (root.as_path(), Runner::Unittest)
        );
        let tests = discovery::project_tests(&root, true).unwrap();
        let select = |name: &str| {
            let test = tests
                .iter()
                .find(|test| test.name() == name)
                .unwrap_or_else(|| panic!("{name} was not discovered"));
            selection(&project, test, None)
        };

        // An exact name: the sibling that shares its prefix stays out, and
        // the flat test directory imports its helpers.
        let chosen = run_python(&select("CalcTests.test_add")).await;
        assert_eq!(chosen.status, "PASSED (1 passed)", "{}", report(&chosen));
        assert!(chosen.success && chosen.output.contains("CHOSEN TEST"));
        assert!(!chosen.output.contains("SIBLING TEST"));

        // A class is all of its tests, subtests and skips included, and a
        // failure leads to its line.
        let class = run_python(&select("CalcTests")).await;
        assert_eq!(
            class.status,
            "FAILED (2 failures, 1 skipped; 4 ran)",
            "{}",
            report(&class)
        );
        let calc = root.join("tests/test_calc.py");
        let failed: Vec<_> = class
            .output
            .lines()
            .filter_map(|line| source_location(line, &root))
            .map(|location| (location.path, location.line))
            .collect();
        assert!(failed.contains(&(calc.clone(), 12)), "{failed:?}");
        assert!(failed.contains(&(calc.clone(), 21)), "{failed:?}");
        assert!(!class.output.contains("OTHER TEST"));

        // A package's tests import as its modules, also from a directory.
        let cart = run_python(&select("CartTests.test_total")).await;
        assert_eq!(cart.status, "PASSED (1 passed)", "{}", report(&cart));
        let package =
            run_python(&directory_selection(&project, &root.join("src/shop/tests"))).await;
        assert_eq!(package.status, "PASSED (1 passed)", "{}", report(&package));
        let all = run_python(&directory_selection(&project, &root.join("tests"))).await;
        assert_eq!(
            all.status,
            "FAILED (2 failures, 1 skipped; 6 ran)",
            "{}",
            report(&all)
        );
        let none = run_python(&directory_selection(&project, &root.join("empty"))).await;
        assert_eq!(
            none.status,
            "NOT RUN: no tests were found in empty",
            "{}",
            report(&none)
        );

        let mut gone = select("CalcTests.test_add");
        gone.args = vec!["-v".into(), "test_calc.CalcTests.test_gone".into()];
        let gone = run_python(&gone).await;
        assert!(gone.status.starts_with("NOT RUN"), "{}", report(&gone));
        assert!(gone.started && !gone.success);

        // A module that does not import fails with where it stopped.
        std::fs::write(
            root.join("tests/test_broken.py"),
            "import unittest\nimport missing_module\n",
        )
        .unwrap();
        let mut broken = select("CalcTests.test_add");
        broken.args = vec!["-v".into(), "test_broken".into()];
        let broken = run_python(&broken).await;
        assert_eq!(
            broken.status,
            "FAILED (1 error; 1 ran)",
            "{}",
            report(&broken)
        );
        assert!(broken
            .output
            .lines()
            .filter_map(|line| source_location(line, &root))
            .any(
                |location| location.path == root.join("tests/test_broken.py") && location.line == 1
            ));
    }

    /// An interpreter with pytest: PYTEST_PYTHON, else python3.
    fn pytest_python() -> PathBuf {
        let python = std::env::var_os("PYTEST_PYTHON").map_or("python3".into(), PathBuf::from);
        let found = std::process::Command::new(&python)
            .args(["-c", "import pytest"])
            .status()
            .is_ok_and(|status| status.success());
        assert!(
            found,
            "{} has no pytest; set PYTEST_PYTHON",
            python.display()
        );
        python
    }

    #[tokio::test]
    #[ignore = "requires pytest (PYTEST_PYTHON or python3) and the Python grammar"]
    async fn real_pytest_selections_and_results() {
        let fixture = python_fixture();
        let root = helix_stdx::path::canonicalize(fixture.path());
        std::fs::create_dir_all(root.join(".venv/bin")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(pytest_python(), root.join(".venv/bin/python")).unwrap();
        std::fs::write(
            root.join("tests/test_plain.py"),
            "import pytest\n\n\n@pytest.mark.parametrize('n', [1, 2])\ndef test_param(n):\n    assert n == 1\n\n\nclass TestPlain:\n    def test_method(self):\n        print('PLAIN TEST')\n",
        )
        .unwrap();
        let project = Project::of(&root.join("tests"));
        assert_eq!(project.runner, Runner::Pytest);
        let tests = discovery::project_tests(&root, true).unwrap();
        let select = |name: &str| {
            let test = tests.iter().find(|test| test.name() == name).unwrap();
            selection(&project, test, None)
        };
        let param = run_python(&select("test_param")).await;
        assert_eq!(
            param.status,
            "FAILED (1 failed, 1 passed)",
            "{}",
            report(&param)
        );
        let lines = lines_of(&param.output);
        let mut index = Index::new(&root);
        let failed: Vec<_> = reported_locations(&lines, &root, &mut index)
            .into_iter()
            .map(|location| (location.path, location.line))
            .collect();
        let plain = root.join("tests/test_plain.py");
        assert_eq!(failed, [(plain.clone(), 5), (plain.clone(), 4)]);
        // pytest also runs what was written for unittest. pytest 9 counts
        // the subtests on their own.
        let class = run_python(&select("CalcTests")).await;
        assert!(
            class.status.starts_with("FAILED (2 failed, ") && class.status.contains("1 skipped"),
            "{}",
            report(&class)
        );
        let method = run_python(&select("TestPlain.test_method")).await;
        assert_eq!(method.status, "PASSED (1 passed)", "{}", report(&method));
        let mut gone = select("TestPlain.test_method");
        gone.args = vec![
            "-v".into(),
            "tests/test_plain.py::TestPlain::test_gone".into(),
        ];
        let gone = run_python(&gone).await;
        assert!(gone.status.starts_with("NOT RUN"), "{}", report(&gone));
    }

    fn lines_of(output: &str) -> Vec<String> {
        output.lines().map(str::to_owned).collect()
    }

    #[tokio::test]
    #[cfg(unix)]
    #[ignore = "requires python3 on PATH; checks that cancelling stops the tests"]
    async fn real_cancel_stops_the_test_process() {
        let fixture = python_fixture();
        let root = helix_stdx::path::canonicalize(fixture.path());
        std::fs::write(root.join("hold"), "").unwrap();
        let project = Project::of(&root.join("tests"));
        let slow = PythonTest {
            file: root.join("tests/test_slow.py"),
            classes: vec!["SlowTests".into()],
            function: Some("test_slow".into()),
            line: 6,
            unittest: true,
        };
        let target = selection(&project, &slow, None);
        let (tx, rx) = watch::channel(false);
        let task = tokio::spawn(async move { run(&target, rx, RUN_TIMEOUT).await });
        let pid_path = root.join("pid");
        tokio::time::timeout(Duration::from_secs(30), async {
            while std::fs::read_to_string(&pid_path).map_or(true, |pid| pid.is_empty()) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let pid: i32 = std::fs::read_to_string(&pid_path).unwrap().parse().unwrap();
        tx.send(true).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.status, "CANCELLED");
        assert!(result.started && !result.success);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
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
        .unwrap();
    }
}

#[cfg(all(test, feature = "integration"))]
mod editor_tests {
    use super::super::harness::*;
    use super::real::python_fixture;
    use super::*;
    use crate::application::Application;

    #[tokio::test(flavor = "multi_thread")]
    async fn commands_need_a_python_file_and_saved_buffers() {
        let fixture = tempfile::tempdir().unwrap();
        let root = helix_stdx::path::canonicalize(fixture.path());
        let source = root.join("tests/test_calc.py");
        std::fs::create_dir(root.join("tests")).unwrap();
        std::fs::write(root.join("pyproject.toml"), "").unwrap();
        std::fs::write(&source, "import unittest\n").unwrap();
        let mut app = test_app(&source);
        for command in ["<space>tp", "<space>tt", "<space>tn"] {
            keys(&mut app, &format!("i# unsaved<esc>{command}")).await;
            assert!(
                status(&app).contains("Save modified files"),
                "{}",
                status(&app)
            );
            keys(&mut app, "u").await;
        }
        // Saved, but without tests at the cursor or in the project.
        keys(&mut app, "<space>tn").await;
        assert!(
            status(&app).contains("Cursor is not inside a test"),
            "{}",
            status(&app)
        );
        keys(&mut app, "<space>tt").await;
        assert!(status(&app).contains("No tests found"), "{}", status(&app));
        assert!(app.editor.test_doc_id.is_none());
        assert!(app.editor.test_last_run.is_none());
        assert!(app.close().await.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn from_a_function_to_its_test() {
        let fixture = python_fixture();
        let root = helix_stdx::path::canonicalize(fixture.path());
        let mut app = test_app(&root.join("src/shop/cart.py"));
        let (view, doc) = current!(app.editor);
        let text = doc.text().to_string();
        let position = text[..text.find("sum(prices)").unwrap()].chars().count();
        doc.set_selection(view.id, helix_core::Selection::point(position));
        keys(&mut app, "gT").await;
        let test = root.join("src/shop/tests/test_cart.py");
        tokio::time::timeout(Duration::from_secs(10), async {
            while doc!(app.editor).path() != Some(&test) {
                keys(&mut app, "").await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{}", status(&app)));
        assert_eq!(super::super::harness::position(&app), (test, 7));
        assert!(app.close().await.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires python3 on PATH and the Python grammar; runs from the cursor to the failing line"]
    async fn cursor_run_rerun_and_navigation_from_the_output() {
        let fixture = python_fixture();
        let root = helix_stdx::path::canonicalize(fixture.path());
        let source = root.join("tests/test_calc.py");
        let mut app = test_app(&source);
        let cursor_on = |app: &mut Application, needle: &str| {
            let (view, doc) = current!(app.editor);
            let text = doc.text().to_string();
            let position = text[..text.find(needle).unwrap()].chars().count();
            doc.set_selection(view.id, helix_core::Selection::point(position));
        };

        cursor_on(&mut app, "self.assertEqual(1 + 2");
        keys(&mut app, "<space>tn").await;
        let output = finished_output(&mut app).await;
        assert!(
            output.starts_with("Python test: test_calc.CalcTests.test_add_more\n"),
            "{output}"
        );
        assert!(output.contains("\nFAILED (1 failure; 1 ran)\n"), "{output}");
        // The source keeps the focus while the results arrive beside it.
        assert_eq!(doc!(app.editor).path(), Some(&source));
        let Some(TestRun::Python(last)) = app.editor.test_last_run.clone() else {
            panic!("no Python run recorded");
        };
        assert_eq!(last.args, ["-v", "test_calc.CalcTests.test_add_more"]);

        // A result opens the test, a frame its line, any other line its test.
        follow(
            &mut app,
            "test_add_more (test_calc.CalcTests.test_add_more) ...",
        )
        .await;
        assert_eq!(position(&app), (source.clone(), 11));
        follow(&mut app, ", line 13, in test_add_more").await;
        assert_eq!(position(&app), (source.clone(), 13));
        follow(&mut app, "AssertionError: 3 != 4").await;
        assert_eq!(position(&app), (source.clone(), 11));
        follow(&mut app, "SIBLING TEST").await;
        assert_eq!(position(&app), (source.clone(), 11));

        // Between the tests of a class the whole class runs, and says why.
        cursor_on(&mut app, "\n    @unittest.skip");
        keys(&mut app, "<space>tn").await;
        let output = finished_output(&mut app).await;
        assert!(
            output.starts_with("Python test: test_calc.CalcTests\n"),
            "{output}"
        );
        assert!(output.contains("Original selection: No test function at the cursor"));

        // The last selection reruns from any buffer, the directory from any of its files.
        app.editor
            .open(&root.join("tests/helpers.py"), Action::Replace)
            .unwrap();
        keys(&mut app, "<space>tl").await;
        let output = finished_output(&mut app).await;
        assert!(
            output.starts_with("Python test: test_calc.CalcTests\n"),
            "{output}"
        );
        keys(&mut app, ":test-package<ret>").await;
        let output = finished_output(&mut app).await;
        assert!(
            output.starts_with("Python test: All tests in tests/\n"),
            "{output}"
        );
        assert!(
            output.contains("\nFAILED (2 failures, 1 skipped; 6 ran)\n"),
            "{output}"
        );
        // Without the cursor on a location, every failure is offered.
        keys(&mut app, "<space>tf").await;
        assert!(app.close().await.is_empty());
    }

    /// An interpreter with debugpy: DEBUGPY_PYTHON, else python3.
    fn debugpy_python() -> PathBuf {
        let python = std::env::var_os("DEBUGPY_PYTHON").map_or("python3".into(), PathBuf::from);
        let found = std::process::Command::new(&python)
            .args(["-c", "import debugpy"])
            .status()
            .is_ok_and(|status| status.success());
        assert!(
            found,
            "{} has no debugpy; set DEBUGPY_PYTHON",
            python.display()
        );
        python
    }

    async fn until(app: &mut Application, what: &str, done: impl Fn(&Application) -> bool) {
        tokio::time::timeout(Duration::from_secs(60), async {
            while !done(app) {
                keys(app, "").await;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}: {}", status(app)));
    }

    fn stopped(app: &Application) -> bool {
        app.editor
            .debug_adapters
            .get_active_client()
            .is_some_and(|client| client.thread_id.is_some())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires debugpy (DEBUGPY_PYTHON or python3), python3 and the Python grammar; debugs a chosen test to a breakpoint and its result"]
    async fn a_debugged_test_stops_at_its_breakpoint_and_reports_its_result() {
        let fixture = python_fixture();
        let root = helix_stdx::path::canonicalize(fixture.path());
        let source = root.join("tests/test_calc.py");
        let python = debugpy_python();
        let mut app = app_with_languages(&source, |languages| {
            let python_language = languages["language"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|language| language["name"].as_str() == Some("python"))
                .unwrap();
            python_language["debugger"]["command"] =
                toml::Value::String(python.to_string_lossy().into_owned());
        });
        let (view, doc) = current!(app.editor);
        let text = doc.text().to_string();
        let breakpoint = text[..text.find("self.assertEqual(double(2), 4)").unwrap()]
            .chars()
            .count();
        doc.set_selection(view.id, helix_core::Selection::point(breakpoint));
        keys(&mut app, "<space>Gb<esc>").await;
        assert_eq!(app.editor.breakpoints[&source].len(), 1);

        // The template, then the test.
        keys(&mut app, "<space>Gltest-func<ret>").await;
        keys(&mut app, "test_add$<ret><esc>").await;
        until(&mut app, "the breakpoint", stopped).await;
        let frame = app.editor.debug_adapters.current_stack_frame().unwrap();
        assert_eq!(frame.line, 9);
        // The tests wait at the breakpoint.
        assert!(app.editor.test_cancel.is_some());
        keys(&mut app, "<space>Gc<esc>").await;
        let output = finished_output(&mut app).await;
        assert!(
            output.starts_with("Python test: test_calc.CalcTests.test_add\n"),
            "{output}"
        );
        assert!(output.contains("The debugger starts the tests"), "{output}");
        assert!(output.contains("\nPASSED (1 passed)\n"), "{output}");
        assert!(output.contains("CHOSEN TEST"), "{output}");
        assert!(!output.contains("SIBLING TEST"), "{output}");
        until(&mut app, "the end of the debug session", |app| {
            app.editor.debug_adapters.is_empty()
        })
        .await;

        // Space G R debugs the same test again; cancelling it ends the session.
        keys(&mut app, "<space>GR<esc>").await;
        until(&mut app, "the breakpoint again", stopped).await;
        keys(&mut app, "<space>tc").await;
        let output = finished_output(&mut app).await;
        assert!(output.contains("\nCANCELLED"), "{output}");
        until(&mut app, "the end of the cancelled session", |app| {
            app.editor.debug_adapters.is_empty()
        })
        .await;
        // Without the debugger, the same selection runs at once.
        keys(&mut app, "<space>tl").await;
        let output = finished_output(&mut app).await;
        assert!(output.contains("\nPASSED (1 passed)\n"), "{output}");
        assert!(
            !output.contains("The debugger starts the tests"),
            "{output}"
        );
        assert!(app.close().await.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires debugpy (DEBUGPY_PYTHON or python3), python3 and the Python grammar; a debugged run ends with its tests and takes what they left along"]
    async fn a_debugged_run_ends_with_its_tests_and_their_leftovers() {
        let fixture = python_fixture();
        let root = helix_stdx::path::canonicalize(fixture.path());
        let source = root.join("tests/test_leftover.py");
        let pid = root.join("leftover-pid");
        std::fs::write(
            &source,
            format!(
                "import subprocess\nimport unittest\n\n\nclass LeftoverTests(unittest.TestCase):\n    def test_leaves_a_process(self):\n        process = subprocess.Popen([\"sleep\", \"300\"])\n        with open({pid:?}, \"w\") as file:\n            file.write(str(process.pid))\n",
                pid = pid.to_string_lossy()
            ),
        )
        .unwrap();
        let python = debugpy_python();
        let mut app = app_with_languages(&source, |languages| {
            let python_language = languages["language"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|language| language["name"].as_str() == Some("python"))
                .unwrap();
            python_language["debugger"]["command"] =
                toml::Value::String(python.to_string_lossy().into_owned());
        });
        keys(&mut app, "<space>Gltest-func<ret>").await;
        keys(&mut app, "test_leaves_a_process<ret><esc>").await;
        // The process holds the output open; the run ends all the same.
        let output = finished_output(&mut app).await;
        assert!(output.contains("\nPASSED (1 passed)\n"), "{output}");
        let pid: i32 = std::fs::read_to_string(&pid).unwrap().parse().unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
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
        .expect("the process the test left is gone");
        assert!(app.close().await.is_empty());
    }
}
