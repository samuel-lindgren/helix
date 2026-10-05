//! .NET tests through `dotnet test`, one project at a time. Tests are found by
//! parsing the project's C# sources; results are read from the console logger,
//! which reports xUnit, NUnit and MSTest alike.
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::Duration,
};

use helix_core::regex::Regex;
use helix_view::{
    editor::{Action, DotnetTestRun, TestRun},
    DocumentId, Editor,
};
use once_cell::sync::Lazy;
use tokio::{process::Command, sync::watch};

use super::{
    check_idle, check_workspace_saved, execute, jump_to_location, pick_location, start, Attach,
    Debugger, Leftovers, RunResult, SourceLocation, Streams, TRUNCATED,
};
use crate::{
    compositor,
    job::Callback,
    ui::{overlay::overlaid, Picker, PickerColumn},
};

mod corresponding;
mod discovery;

pub(in crate::commands) use corresponding::corresponding_tests;
use discovery::DotnetTest;

pub(super) const BUFFER_NAME: &str = "[dotnet-test]";
/// The first line of the output buffer starts with this.
pub(super) const TITLE: &str = ".NET test: ";
// Restoring packages and building the referenced projects come before the tests.
pub(super) const RUN_TIMEOUT: Duration = Duration::from_secs(600);
/// Every result is listed, not only the failures, so that a run shows what it ran.
const LOGGER: &str = "console;verbosity=normal";

/// The project a file belongs to: the project file itself, or the only one in
/// the nearest directory that has any.
fn project_of(path: &Path) -> anyhow::Result<PathBuf> {
    let is_project = |path: &Path| path.extension().is_some_and(|e| e == "csproj");
    if is_project(path) {
        return Ok(path.to_owned());
    }
    for dir in path.ancestors().skip(1) {
        let mut projects: Vec<_> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| is_project(path) && path.is_file())
            .collect();
        match projects.len() {
            0 => continue,
            1 => return Ok(projects.remove(0)),
            _ => anyhow::bail!(
                "Several project files in {}: open the one to test",
                dir.display()
            ),
        }
    }
    anyhow::bail!("No project file (*.csproj) at or above {}", path.display())
}

/// Where the projects that a test project builds can be: the repository,
/// else the directory of a solution, else the project alone. A solution can
/// sit beside the tests and name projects that are not below it.
fn workspace_root(project: &Path) -> PathBuf {
    let dir = project.parent().unwrap_or(project);
    let has_solution = |dir: &&Path| {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .any(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|e| e == "sln" || e == "slnx")
            })
    };
    dir.ancestors()
        .find(|dir| dir.join(".git").exists())
        .or_else(|| dir.ancestors().find(has_solution))
        .unwrap_or(dir)
        .to_owned()
}

/// The project of the focused file, once nothing that it builds is unsaved.
fn saved_project(editor: &Editor) -> anyhow::Result<PathBuf> {
    let path = doc!(editor)
        .path()
        .ok_or_else(|| anyhow::anyhow!("Open a saved C# file to run its tests"))?;
    let project = project_of(path)?;
    check_workspace_saved(editor, &workspace_root(&project))?;
    Ok(project)
}

pub(super) fn package(cx: &mut compositor::Context, debugger: Option<Debugger>) {
    match saved_project(cx.editor) {
        Ok(project) => start(cx, TestRun::Dotnet(project_selection(&project)), debugger),
        Err(err) => cx.editor.set_error(err.to_string()),
    }
}

/// With a debugger, the chosen test runs under it.
pub(super) fn pick(cx: &mut compositor::Context, debugger: Option<Debugger>) {
    let project = match check_idle(cx.editor).and_then(|()| saved_project(cx.editor)) {
        Ok(project) => project,
        Err(err) => {
            cx.editor.set_error(err.to_string());
            return;
        }
    };
    let dir = project.parent().unwrap().to_owned();
    cx.jobs.callback(async move {
        let scan = dir.clone();
        // A large project is many files to parse.
        let tests = tokio::task::spawn_blocking(move || discovery::project_tests(&scan)).await?;
        Ok(Callback::EditorCompositor(Box::new(
            move |editor, compositor| {
                let tests = match tests {
                    Ok(tests) if tests.is_empty() => {
                        editor.set_error(format!(
                            "No tests found in the sources of {}",
                            project.file_name().unwrap().to_string_lossy()
                        ));
                        return;
                    }
                    Ok(tests) => tests,
                    Err(err) => {
                        editor.set_error(err.to_string());
                        return;
                    }
                };
                let all = tests.clone();
                let picker = Picker::new(
                    [
                        PickerColumn::new("test", |t: &DotnetTest, _: &PathBuf| {
                            t.short_name().into()
                        }),
                        PickerColumn::new("namespace", |t: &DotnetTest, _: &PathBuf| {
                            t.namespace().into()
                        }),
                        PickerColumn::new("file", |t: &DotnetTest, dir: &PathBuf| {
                            t.file
                                .strip_prefix(dir)
                                .unwrap_or(&t.file)
                                .to_string_lossy()
                                .into_owned()
                                .into()
                        }),
                    ],
                    0,
                    tests,
                    dir,
                    move |cx, test, _| {
                        let target = TestRun::Dotnet(selection(&project, test, None, &all));
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
    let target = (|| -> anyhow::Result<_> {
        check_idle(cx.editor)?;
        let (view, doc) = current_ref!(cx.editor);
        let path = doc
            .path()
            .filter(|path| path.extension().is_some_and(|e| e == "cs"))
            .ok_or_else(|| anyhow::anyhow!("Open a saved C# file with tests"))?;
        let project = saved_project(cx.editor)?;
        let source = doc.text().to_string();
        let byte = doc.text().char_to_byte(
            doc.selection(view.id)
                .primary()
                .cursor(doc.text().slice(..)),
        );
        let declared = discovery::parse(discovery::grammar()?, &source, path);
        let (test, note) = at_cursor(&declared, &source, byte)?;
        // Only a class is selected among the others of the project.
        let others = match test.method {
            Some(_) => Vec::new(),
            None => discovery::project_tests(project.parent().unwrap_or(&project))?,
        };
        Ok(selection(&project, test, note, &others))
    })();
    match target {
        Ok(target) => start(cx, TestRun::Dotnet(target), None),
        Err(err) => cx.editor.set_error(err.to_string()),
    }
}

/// The test method around the cursor, else the test class around it. A class
/// is run with a note, because more runs than the cursor is on.
fn at_cursor<'a>(
    declared: &'a [discovery::Declared],
    source: &str,
    byte: usize,
) -> anyhow::Result<(&'a DotnetTest, Option<String>)> {
    // Leading indentation belongs to the declaration on this line. Do not
    // cross a newline and choose the next test.
    let byte = byte
        + source[byte..]
            .bytes()
            .take_while(|b| matches!(b, b' ' | b'\t'))
            .count();
    // A nested declaration comes after the one it is in.
    let innermost = |method: bool| {
        declared
            .iter()
            .rev()
            .find(|d| d.test.method.is_some() == method && d.bytes.contains(&byte))
            .map(|d| &d.test)
    };
    if let Some(test) = innermost(true) {
        return Ok((test, None));
    }
    let class = innermost(false)
        .ok_or_else(|| anyhow::anyhow!("Cursor is not inside a test method or a test class"))?;
    let note = format!(
        "No test method at the cursor; running all tests of {}.",
        class.name()
    );
    Ok((class, Some(note)))
}

/// `tests` are those of the project, among which a class has to be told apart.
fn selection(
    project: &Path,
    test: &DotnetTest,
    note: Option<String>,
    tests: &[DotnetTest],
) -> DotnetTestRun {
    DotnetTestRun {
        project: project.to_owned(),
        workspace: workspace_root(project),
        name: test.name(),
        filter: Some(test.filter(tests.iter().map(|test| test.class.as_str()))),
        selection_note: note,
    }
}

fn project_selection(project: &Path) -> DotnetTestRun {
    DotnetTestRun {
        project: project.to_owned(),
        workspace: workspace_root(project),
        name: "All tests in project".into(),
        filter: None,
        selection_note: None,
    }
}

/// The test host prints its process id and waits until a debugger is attached,
/// which it then does not break into by itself.
const HOST_DEBUG: [(&str, &str); 2] = [("VSTEST_HOST_DEBUG", "1"), ("VSTEST_DEBUG_NOBP", "1")];

/// The lines above the result in the output buffer.
pub(super) fn header(target: &DotnetTestRun, debug: bool) -> String {
    let notice = target
        .selection_note
        .as_ref()
        .map(|note| format!("Original selection: {note}\n"))
        .unwrap_or_default();
    let filter = target
        .filter
        .as_ref()
        .map(|filter| format!(" --filter {filter:?}"))
        .unwrap_or_default();
    let (environment, sequential, debugger) = if debug {
        let environment: String = HOST_DEBUG
            .iter()
            .map(|(name, value)| format!("{name}={value} "))
            .collect();
        (
            environment,
            " -p:TestTfmsInParallel=false",
            "The debugger attaches when the test host has started; the results follow when it is done.\n",
        )
    } else {
        Default::default()
    };
    format!(
        "{TITLE}{}\nProject: {}\nCommand: {environment}dotnet test {:?} --nologo --logger {LOGGER:?}{filter}{sequential}\n\
         Tests read saved files from disk. Save and rerun after edits.\n{debugger}\
         Space t f: go to this line's test or source | Space t r: results | Space t c: cancel\n{notice}\n",
        target.name,
        target.project.display(),
        target.project.file_name().unwrap_or_default().to_string_lossy(),
    )
}

/// Watches the output of a run for test hosts that wait for a debugger:
/// `Process Id: 12345, Name: dotnet`. A project with several target
/// frameworks starts one host after the other.
struct HostWatch {
    attach: Attach,
    /// The start of a line whose end has not arrived yet.
    partial: Vec<u8>,
    last: Option<u32>,
}

static HOST: Lazy<Regex> = Lazy::new(|| Regex::new(r"^Process Id: ([0-9]+), Name: ").unwrap());

impl HostWatch {
    fn new(attach: Attach) -> Self {
        Self {
            attach,
            partial: Vec::new(),
            last: None,
        }
    }

    /// Takes the next piece of the output.
    fn see(&mut self, piece: &[u8]) {
        for byte in piece {
            if *byte != b'\n' {
                // An announcement is short: more than this is not one.
                if self.partial.len() < 256 {
                    self.partial.push(*byte);
                }
                continue;
            }
            let line = String::from_utf8_lossy(&self.partial);
            let pid = HOST.captures(&line).and_then(|host| host[1].parse().ok());
            // A host repeats its announcement while it waits.
            if pid.is_some() && pid != self.last {
                self.last = pid;
                (self.attach)(pid.unwrap());
            }
            self.partial.clear();
        }
    }
}

/// With `attach`, the tests wait for a debugger, and `attach` is given the
/// process to attach it to.
pub(super) async fn run(
    program: &Path,
    target: &DotnetTestRun,
    cancel: watch::Receiver<bool>,
    deadline: Duration,
    attach: Option<Attach>,
) -> RunResult {
    let mut command = Command::new(program);
    command
        .arg("test")
        .arg(&target.project)
        .args(["--nologo", "--logger", LOGGER]);
    if let Some(filter) = &target.filter {
        command.args(["--filter", filter]);
    }
    command
        .current_dir(target.project.parent().unwrap_or(Path::new(".")))
        // The results are read from the English output.
        .env("DOTNET_CLI_UI_LANGUAGE", "en")
        .env("DOTNET_NOLOGO", "1")
        // The terminal logger redraws lines instead of printing them, and
        // colours would stand before the words that the results are read by.
        .env("MSBUILDTERMINALLOGGER", "off")
        .env_remove("DOTNET_SYSTEM_CONSOLE_ALLOW_ANSI_COLOR_REDIRECTION");
    let mut hosts = attach.map(HostWatch::new);
    if hosts.is_some() {
        // One debugger at a time: the test hosts of a project with several
        // target frameworks would otherwise all wait at once.
        command.arg("-p:TestTfmsInParallel=false").envs(HOST_DEBUG);
    }
    // Results go to one stream and failure announcements to the other: only
    // together, in the order written, do they read as one report. The compiler
    // server that the build leaves running saves the next run its start.
    let execution = execute(
        command,
        cancel,
        deadline,
        Streams::Merged,
        Leftovers::Kept,
        |output| {
            if let Some(hosts) = &mut hosts {
                hosts.see(output);
            }
        },
    )
    .await;
    let execution = match execution {
        Ok(execution) => execution,
        Err(err) => {
            return RunResult {
                started: false,
                status: "COULD NOT START dotnet test".into(),
                output: format!("{err}\n"),
                success: false,
            }
        }
    };
    let mut output = String::from_utf8_lossy(&execution.stdout).into_owned();
    let summary = Summary::read(&output);
    if execution.truncated {
        output.push_str(TRUNCATED);
    }
    let (status, success) = match execution.exit {
        Err(reason) => (reason, false),
        Ok(exit) if !exit.success() => {
            let status = if summary.failed > 0 {
                format!("FAILED ({summary})")
            } else if summary.total == 0 && summary.build_errors {
                format!("BUILD FAILED ({exit})")
            } else {
                format!("FAILED ({exit})")
            };
            (status, false)
        }
        Ok(_) if summary.total == 0 => {
            let status = if target.filter.is_some() {
                "NOT RUN: selected test was not observed (renamed, excluded from the build, declared in a base class, or in a generic or parameterized class)"
            } else {
                "NOT RUN: no tests were observed (is this a test project?)"
            };
            (status.into(), false)
        }
        Ok(_) if summary.passed == 0 => (format!("SKIPPED ({summary})"), false),
        Ok(_) => (format!("PASSED ({summary})"), true),
    };
    RunResult {
        started: true,
        status,
        output,
        success,
    }
}

/// The totals that `dotnet test` prints after each test assembly, added up.
#[derive(Default, Debug, PartialEq, Eq)]
struct Summary {
    total: usize,
    passed: usize,
    failed: usize,
    skipped: usize,
    build_errors: bool,
}

/// What `dotnet test` prints after each test assembly:
///
/// ```text
/// Total tests: 7
///      Passed: 3
///      Failed: 3
///     Skipped: 1
///  Total time: 1.6186 Seconds
/// ```
///
/// Only a whole block counts: a test can print a line that looks like one of these.
static TOTALS_BLOCK: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?m)^Total tests: ([0-9]+)\r?\n(?: +Passed: ([0-9]+)\r?\n)?(?: +Failed: ([0-9]+)\r?\n)?(?: +Skipped: ([0-9]+)\r?\n)? *Total time: ",
    )
    .unwrap()
});
/// An error of the compiler, of MSBuild or of the package restore:
/// `File.cs(1,2): error CS0103: …`, `Shop.csproj : error NU1101: …`.
static BUILD_ERROR: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?m)(?:^|\s)error [A-Z]+[0-9]+: ").unwrap());

impl Summary {
    fn read(output: &str) -> Self {
        let mut summary = Self {
            build_errors: BUILD_ERROR.is_match(output),
            ..Self::default()
        };
        for block in TOTALS_BLOCK.captures_iter(output) {
            let number = |group| {
                block
                    .get(group)
                    .and_then(|number| number.as_str().parse::<usize>().ok())
                    .unwrap_or(0)
            };
            summary.total += number(1);
            summary.passed += number(2);
            summary.failed += number(3);
            summary.skipped += number(4);
        }
        if summary.total == 0 {
            // No totals follow a run in which nothing passed or failed, or one
            // that was cut short. The results it listed are what there is.
            for result in output.lines().filter_map(|line| RESULT.captures(line)) {
                summary.total += 1;
                match &result[1] {
                    "Passed" => summary.passed += 1,
                    "Failed" => summary.failed += 1,
                    _ => summary.skipped += 1,
                }
            }
        }
        summary
    }
}

impl std::fmt::Display for Summary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let parts: Vec<_> = [
            (self.failed, "failed"),
            (self.passed, "passed"),
            (self.skipped, "skipped"),
        ]
        .into_iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, what)| format!("{count} {what}"))
        .collect();
        f.write_str(&parts.join(", "))
    }
}

/// A stack frame with source information:
/// `at Shop.Calc.Add(Int32 a) in /src/Calc.cs:line 12`. NUnit numbers its frames.
static FRAME: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^\s*(?:[0-9]+\)\s+)?(at .+?) in (.+):line ([0-9]+)\s*$").unwrap());
/// A compiler or analyzer diagnostic, `/src/Calc.cs(12,5): error CS0103: … [/src/Shop.csproj]`,
/// which is also how xUnit writes a stack frame.
static DIAGNOSTIC: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^(?:\[xUnit\.net [^\]]*\])?\s*(\S.*?)\(([0-9]+)(?:,[0-9]+)*\): (.*?)(?: \[[^\]]+proj\])?\s*$",
    )
    .unwrap()
});
/// A result of the console logger: `  Failed Shop.Tests.CalcTests.Adds [12 ms]`.
static RESULT: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^  (Passed|Failed|Skipped) (\S.*?)(?: \[[^\]]*\])?\s*$").unwrap());
/// xUnit announces a failure before the logger reports it:
/// `[xUnit.net 00:00:00.50]     Shop.Tests.CalcTests.Fails [FAIL]`.
static ANNOUNCED: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^\[xUnit\.net [^\]]*\]\s+(\S.*?) \[(?:FAIL|SKIP)\]\s*$").unwrap());
/// The totals of a test assembly, which belong to no single test.
static TOTALS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?:Test Run \w+\.|\s*(?:Total tests|Passed|Failed|Skipped|Total time): .*)$")
        .unwrap()
});

/// The location an output line names. Tools print absolute paths; anything
/// else that happens to have the same shape is not a location.
fn source_location(line: &str) -> Option<SourceLocation> {
    let (path, number, message) = if let Some(frame) = FRAME.captures(line) {
        (
            frame.get(2)?.as_str(),
            frame.get(3)?.as_str(),
            frame.get(1)?.as_str(),
        )
    } else {
        let diagnostic = DIAGNOSTIC.captures(line)?;
        (
            diagnostic.get(1)?.as_str(),
            diagnostic.get(2)?.as_str(),
            diagnostic.get(3)?.as_str(),
        )
    };
    let path = Path::new(path);
    let line = number.parse::<usize>().ok()?.checked_sub(1)?;
    path.is_absolute().then(|| SourceLocation {
        path: helix_stdx::path::canonicalize(path),
        line,
        message: message.to_owned(),
    })
}

/// The test an output line belongs to: named on the line, else by the closest
/// result or announcement above it.
fn test_on_line(lines: &[String], line: usize) -> Option<&str> {
    if TOTALS.is_match(&lines[line]) {
        return None;
    }
    lines[..=line].iter().rev().find_map(|line| {
        let captures = RESULT.captures(line).map(|c| c.get(2));
        let captures = captures.or_else(|| ANNOUNCED.captures(line).map(|c| c.get(1)));
        Some(captures??.as_str())
    })
}

/// Where the test of a reported name is declared. xUnit reports the full
/// name, NUnit and MSTest the method alone, and all of them add the arguments
/// of a data row. Several declarations can share a method name.
fn declarations(reported: &str, tests: &[DotnetTest]) -> Vec<SourceLocation> {
    let name = reported.split('(').next().unwrap_or(reported).trim_end();
    let methods = || tests.iter().filter(|test| test.method.is_some());
    let mut found: Vec<_> = methods().filter(|test| test.name() == name).collect();
    if found.is_empty() {
        let method = name.rsplit('.').next().unwrap_or(name);
        found = methods()
            .filter(|test| test.method.as_deref() == Some(method))
            .collect();
    }
    found
        .into_iter()
        .map(|test| SourceLocation {
            path: test.file.clone(),
            line: test.line,
            message: test.name(),
        })
        .collect()
}

fn project_file(lines: &[String]) -> Option<PathBuf> {
    lines
        .iter()
        .take_while(|line| !line.is_empty())
        .find_map(|line| line.strip_prefix("Project: "))
        .map(PathBuf::from)
}

/// The tests in the sources of the project that the output is of.
fn tests_of(lines: &[String]) -> Vec<DotnetTest> {
    project_file(lines)
        .and_then(|project| discovery::project_tests(project.parent()?).ok())
        .unwrap_or_default()
}

/// The .NET reading of [`super::show_locations`].
pub(super) fn show_locations(cx: &mut compositor::Context, id: DocumentId) {
    let lines: Vec<String> = cx.editor.documents[&id]
        .text()
        .lines()
        .map(|line| line.to_string().trim_end().to_owned())
        .collect();
    let (view, doc) = current_ref!(cx.editor);
    if doc.id() == id {
        let line = doc
            .selection(view.id)
            .primary()
            .cursor_line(doc.text().slice(..));
        // Frames of the frameworks name files of the machine they were built on.
        if let Some(location) =
            source_location(&lines[line]).filter(|location| location.path.is_file())
        {
            jump_to_location(cx.editor, &location, Action::Replace);
            return;
        }
        if let Some(test) = test_on_line(&lines, line) {
            match <[_; 1]>::try_from(declarations(test, &tests_of(&lines))) {
                Ok([location]) => jump_to_location(cx.editor, &location, Action::Replace),
                Err(locations) if locations.is_empty() => cx.editor.set_error(format!(
                    "{test} is not declared in the project's sources under that name"
                )),
                Err(locations) => pick_location(cx, locations),
            }
            return;
        }
    }
    let locations = reported_locations(&lines, &tests_of(&lines));
    if locations.is_empty() {
        cx.editor.set_error(
            "No source locations or failed tests reported; Space t r shows the full output",
        );
        return;
    }
    pick_location(cx, locations);
}

/// Every source line the output names, in the order named, and where each
/// failed test is declared.
fn reported_locations(lines: &[String], tests: &[DotnetTest]) -> Vec<SourceLocation> {
    let mut locations = Vec::new();
    let mut seen = HashSet::new();
    for line in lines {
        let found = if let Some(location) = source_location(line) {
            if location.path.is_file() {
                vec![location]
            } else {
                Vec::new()
            }
        } else if let Some(failed) = RESULT.captures(line).filter(|c| &c[1] == "Failed") {
            declarations(&failed[2], tests)
                .into_iter()
                .map(|location| SourceLocation {
                    message: format!("Failed {}", &failed[2]),
                    ..location
                })
                .collect()
        } else {
            Vec::new()
        };
        for location in found {
            // xUnit and the logger each print the frames of a failure.
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

    fn test(class: &str, method: &str, file: &str, line: usize) -> DotnetTest {
        DotnetTest {
            class: class.into(),
            method: (!method.is_empty()).then(|| method.to_owned()),
            file: file.into(),
            line,
        }
    }

    const XUNIT: &str = "\
.NET test: All tests in project
Project: /src/Shop.Tests/Shop.Tests.csproj

FAILED (2 failed, 1 passed, 1 skipped)

  Determining projects to restore...
/src/Shop.Tests/CalcTests.cs(10,21): warning CS0219: The variable 'x' is assigned but its value is never used [/src/Shop.Tests/Shop.Tests.csproj]
  Shop.Tests -> /src/Shop.Tests/bin/Debug/net10.0/Shop.Tests.dll
Test run for /src/Shop.Tests/bin/Debug/net10.0/Shop.Tests.dll (.NETCoreApp,Version=v10.0)
[xUnit.net 00:00:00.44]   Starting:    Shop.Tests
console output line
[xUnit.net 00:00:00.56]     Shop.Tests.CalcTests.AddsTable(a: 2, b: 2, sum: 5) [FAIL]
[xUnit.net 00:00:00.56]       Assert.Equal() Failure: Values differ
[xUnit.net 00:00:00.56]       Stack Trace:
[xUnit.net 00:00:00.56]         /src/Shop.Tests/CalcTests.cs(32,0): at Shop.Tests.CalcTests.AddsTable(Int32 a, Int32 b, Int32 sum)
[xUnit.net 00:00:00.59]   Finished:    Shop.Tests
  Failed Shop.Tests.CalcTests.AddsTable(a: 2, b: 2, sum: 5) [15 ms]
  Error Message:
   Assert.Equal() Failure: Values differ
Expected: 5
  Stack Trace:
     at Shop.Tests.CalcTests.AddsTable(Int32 a, Int32 b, Int32 sum) in /src/Shop.Tests/CalcTests.cs:line 32
   at System.Reflection.MethodBaseInvoker.InvokeWithNoArgs(Object obj, BindingFlags invokeAttr)
  Passed Shop.Tests.CalcTests.AddsTable(a: 1, b: 2, sum: 3) [2 ms]
  Skipped Shop.Tests.CalcTests.Skipped [1 ms]
  Failed Shop.Tests.CalcTests+Nested.Inner [< 1 ms]
  Error Message:
   boom

Test Run Failed.
Total tests: 4
     Passed: 1
     Failed: 2
    Skipped: 1
 Total time: 1.7841 Seconds
";

    fn lines(output: &str) -> Vec<String> {
        output.lines().map(str::to_owned).collect()
    }

    #[test]
    fn summaries_add_up_over_assemblies_and_name_what_happened() {
        let summary = Summary::read(XUNIT);
        assert_eq!(
            summary,
            Summary {
                total: 4,
                passed: 1,
                failed: 2,
                skipped: 1,
                build_errors: false
            }
        );
        assert_eq!(summary.to_string(), "2 failed, 1 passed, 1 skipped");
        let twice = Summary::read(&format!(
            "{XUNIT}\nTotal tests: 3\n     Passed: 3\n Total time: 0.9 Seconds\n"
        ));
        assert_eq!((twice.total, twice.passed, twice.failed), (7, 4, 2));
        assert_eq!(
            Summary {
                passed: 3,
                total: 3,
                ..Default::default()
            }
            .to_string(),
            "3 passed"
        );
        // Errors of the compiler and of the restore, which names no line.
        for error in [
            "/src/A.cs(54,42): error CS0103: The name 'x' does not exist [/src/A.csproj]\n",
            "/src/A.csproj : error NU1101: Unable to find package Nope. [/src/A.csproj]\n",
            "MSBUILD : error MSB1009: Project file does not exist.\n",
        ] {
            let broken = Summary::read(error);
            assert!(broken.build_errors && broken.total == 0, "{error}");
        }
        assert!(!Summary::read("  Failed Shop.Tests.Parses_error_codes [1 ms]\n").build_errors);
        // What a test prints is not a total: only the whole block is.
        let printed = Summary::read(&XUNIT.replace(
            "console output line\n",
            "Total tests: 100\n     Failed: 7\nTotal tests: 5\n",
        ));
        assert_eq!((printed.total, printed.failed), (4, 2));
        // Without totals the listed results count: NUnit prints none for a
        // run in which nothing passed or failed.
        let inconclusive = Summary::read("  Skipped Inconclusive [23 ms]\n\n");
        assert_eq!((inconclusive.total, inconclusive.skipped), (1, 1));
        let cut_short = Summary::read("  Passed A [1 ms]\n  Failed B [2 ms]\nThe active test run was aborted.\nTotal tests: Unknown\n");
        assert_eq!(
            (cut_short.total, cut_short.passed, cut_short.failed),
            (2, 1, 1)
        );
    }

    #[test]
    fn locations_in_frames_and_diagnostics() {
        for (text, file, line, message) in [
            (
                "     at Shop.Tests.CalcTests.Fails() in /src/Shop Tests/CalcTests.cs:line 21",
                "/src/Shop Tests/CalcTests.cs",
                20,
                "at Shop.Tests.CalcTests.Fails()",
            ),
            (
                "1)    at Shop.Tests.CalcTests.AddsTable(Int32 a, Int32 b) in /src/CalcTests.cs:line 26",
                "/src/CalcTests.cs",
                25,
                "at Shop.Tests.CalcTests.AddsTable(Int32 a, Int32 b)",
            ),
            (
                "/src/CalcTests.cs(54,42): error CS0103: The name 'x' does not exist in the current context [/src/Shop.Tests.csproj]",
                "/src/CalcTests.cs",
                53,
                "error CS0103: The name 'x' does not exist in the current context",
            ),
            (
                "[xUnit.net 00:00:00.56]         /src/CalcTests.cs(32,0): at Shop.Tests.CalcTests.AddsTable(Int32 a)",
                "/src/CalcTests.cs",
                31,
                "at Shop.Tests.CalcTests.AddsTable(Int32 a)",
            ),
            (
                "/src/a (copy)/Calc.cs(3,1,3,9): warning CA1000: message",
                "/src/a (copy)/Calc.cs",
                2,
                "warning CA1000: message",
            ),
        ] {
            let location = source_location(text).unwrap_or_else(|| panic!("{text}"));
            assert_eq!(location.path, Path::new(file), "{text}");
            assert_eq!(location.line, line, "{text}");
            assert_eq!(location.message, message, "{text}");
        }
        for text in [
            "   at System.Reflection.MethodBaseInvoker.InvokeWithNoArgs(Object obj, BindingFlags invokeAttr)",
            "  Failed AddsTable (2,2,5) [22 ms]",
            "  Passed AddsTable(1,2,3) [< 1 ms]",
            "     Assert.That(a + b, Is.EqualTo(sum))",
            "Shop.Tests -> relative/path.cs(1,2): not absolute",
            "/src/CalcTests.cs(0,0): error X: line zero",
            "Total tests: 4",
        ] {
            assert!(source_location(text).is_none(), "{text}");
        }
    }

    #[test]
    fn output_lines_belong_to_the_result_or_announcement_above_them() {
        let lines = lines(XUNIT);
        let owner = |needle: &str| {
            let line = lines.iter().position(|l| l.contains(needle)).unwrap();
            test_on_line(&lines, line)
        };
        assert_eq!(
            project_file(&lines),
            Some("/src/Shop.Tests/Shop.Tests.csproj".into())
        );
        for outside in [
            ".NET test:",
            "FAILED (2 failed",
            "warning CS0219",
            "Starting:",
            "console output line",
            "Test Run Failed.",
            "Total tests: 4",
            "     Failed: 2",
            "Total time:",
        ] {
            assert_eq!(owner(outside), None, "{outside}");
        }
        let table = Some("Shop.Tests.CalcTests.AddsTable(a: 2, b: 2, sum: 5)");
        assert_eq!(owner("[FAIL]"), table);
        assert_eq!(owner("00.56]       Stack Trace:"), table);
        assert_eq!(owner("  Failed Shop.Tests.CalcTests.AddsTable"), table);
        assert_eq!(owner("Expected: 5"), table);
        assert_eq!(owner("MethodBaseInvoker"), table);
        assert_eq!(
            owner("  Passed "),
            Some("Shop.Tests.CalcTests.AddsTable(a: 1, b: 2, sum: 3)")
        );
        assert_eq!(owner("  Skipped "), Some("Shop.Tests.CalcTests.Skipped"));
        assert_eq!(owner("boom"), Some("Shop.Tests.CalcTests+Nested.Inner"));
        // NUnit and MSTest: the method alone, with or without a duration.
        let other = lines_of(&["  Skipped Skipped", "  Passed AddsTable (1,2,3) [11 ms]"]);
        assert_eq!(test_on_line(&other, 0), Some("Skipped"));
        assert_eq!(test_on_line(&other, 1), Some("AddsTable (1,2,3)"));
    }

    fn lines_of(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|line| (*line).to_owned()).collect()
    }

    #[test]
    fn reported_names_resolve_to_declarations() {
        let tests = [
            test("Shop.Tests.CalcTests", "", "/src/CalcTests.cs", 4),
            test("Shop.Tests.CalcTests", "AddsTable", "/src/CalcTests.cs", 27),
            test("Shop.Tests.CalcTests", "Adds", "/src/CalcTests.cs", 10),
            test(
                "Shop.Tests.CalcTests+Nested",
                "Inner",
                "/src/CalcTests.cs",
                44,
            ),
            test("Shop.Tests.Other", "Adds", "/src/Other.cs", 6),
            test("Shop.Tests.Base", "Shared", "/src/Base.cs", 8),
        ];
        let found = |reported: &str| -> Vec<(String, usize)> {
            declarations(reported, &tests)
                .into_iter()
                .map(|l| (l.path.to_string_lossy().into_owned(), l.line))
                .collect()
        };
        let calc = |line| ("/src/CalcTests.cs".to_owned(), line);
        // xUnit: the full name, data rows with their arguments.
        assert_eq!(found("Shop.Tests.CalcTests.Adds"), [calc(10)]);
        assert_eq!(
            found("Shop.Tests.CalcTests.AddsTable(a: 2, b: 2, sum: 5)"),
            [calc(27)]
        );
        assert_eq!(found("Shop.Tests.CalcTests+Nested.Inner"), [calc(44)]);
        // NUnit and MSTest: the method, which more than one class may declare.
        assert_eq!(found("AddsTable(2,2,5)"), [calc(27)]);
        assert_eq!(found("AddsTable (2,2,5)"), [calc(27)]);
        assert_eq!(found("Adds"), [calc(10), ("/src/Other.cs".to_owned(), 6)]);
        // Inherited: reported under the derived class, declared in the base.
        assert_eq!(
            found("Shop.Tests.Derived.Shared"),
            [("/src/Base.cs".to_owned(), 8)]
        );
        // A class is not a result, and a display name is no declaration.
        assert!(found("Shop.Tests.CalcTests").is_empty());
        assert!(found("adds two numbers").is_empty());
    }

    #[test]
    fn the_cursor_selects_a_method_else_its_class() {
        let source = "namespace N;\nclass T {\n    [Fact]\n    public void A() { }\n\n    class Inner {\n        [Fact] public void B() { }\n    }\n}\nclass Plain { }\n";
        let range = |needle: &str| {
            let start = source.find(needle).unwrap();
            start..start + needle.len()
        };
        let declared = [
            ("N.T", "", range("class T {\n    [Fact]\n    public void A() { }\n\n    class Inner {\n        [Fact] public void B() { }\n    }\n}")),
            ("N.T", "A", range("[Fact]\n    public void A() { }")),
            ("N.T+Inner", "", range("class Inner {\n        [Fact] public void B() { }\n    }")),
            ("N.T+Inner", "B", range("[Fact] public void B() { }")),
        ]
        .map(|(class, method, bytes)| discovery::Declared {
            test: test(class, method, "/src/T.cs", 0),
            bytes,
        });
        let at = |needle: &str| {
            at_cursor(&declared, source, source.find(needle).unwrap())
                .map(|(test, note)| (test.name(), note.is_some()))
                .map_err(|err| err.to_string())
        };
        assert_eq!(at("public void A"), Ok(("N.T.A".into(), false)));
        // Indentation before the attribute belongs to the declaration.
        assert_eq!(
            at("    [Fact]\n    public void A"),
            Ok(("N.T.A".into(), false))
        );
        assert_eq!(at("{ }\n\n    class Inner"), Ok(("N.T.A".into(), false)));
        assert_eq!(at("public void B"), Ok(("N.T+Inner.B".into(), false)));
        assert_eq!(at("class Inner"), Ok(("N.T+Inner".into(), true)));
        assert_eq!(at("class T"), Ok(("N.T".into(), true)));
        // The empty line between the members of T.
        assert_eq!(at("\n    class Inner"), Ok(("N.T".into(), true)));
        assert!(at("namespace N").is_err());
        assert!(at("class Plain").is_err());
    }

    #[test]
    fn failures_and_frames_are_listed_once_where_the_files_exist() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("CalcTests.cs");
        std::fs::write(&file, "\n".repeat(60)).unwrap();
        let file = helix_stdx::path::canonicalize(&file);
        let path = file.to_string_lossy();
        let output = XUNIT.replace("/src/Shop.Tests/CalcTests.cs", &path);
        let tests = [
            test("Shop.Tests.CalcTests", "AddsTable", &path, 27),
            test("Shop.Tests.CalcTests+Nested", "Inner", &path, 44),
        ];
        let found: Vec<_> = reported_locations(&lines(&output), &tests)
            .into_iter()
            .map(|location| {
                assert_eq!(location.path, file);
                (location.line, location.message)
            })
            .collect();
        assert_eq!(
            found,
            [
                (
                    9,
                    "warning CS0219: The variable 'x' is assigned but its value is never used"
                        .to_owned()
                ),
                (
                    31,
                    "at Shop.Tests.CalcTests.AddsTable(Int32 a, Int32 b, Int32 sum)".to_owned()
                ),
                (
                    27,
                    "Failed Shop.Tests.CalcTests.AddsTable(a: 2, b: 2, sum: 5)".to_owned()
                ),
                (44, "Failed Shop.Tests.CalcTests+Nested.Inner".to_owned()),
            ]
        );
        // The files of the original output do not exist here.
        assert!(reported_locations(&lines(XUNIT), &[]).is_empty());
    }

    #[test]
    fn projects_and_workspaces_are_found_upwards() {
        let root = tempfile::tempdir().unwrap();
        let root = helix_stdx::path::canonicalize(root.path());
        for file in [
            "Shop.sln",
            "src/Shop/Shop.csproj",
            "src/Shop/Calc.cs",
            "tests/Shop.Tests/Shop.Tests.csproj",
            "tests/Shop.Tests/Unit/CalcTests.cs",
            "tests/Two/A.csproj",
            "tests/Two/B.csproj",
            "tests/Two/T.cs",
            "Loose.cs",
        ] {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        let project = root.join("tests/Shop.Tests/Shop.Tests.csproj");
        assert_eq!(
            project_of(&root.join("tests/Shop.Tests/Unit/CalcTests.cs")).unwrap(),
            project
        );
        assert_eq!(project_of(&project).unwrap(), project);
        assert_eq!(
            project_of(&root.join("src/Shop/Calc.cs")).unwrap(),
            root.join("src/Shop/Shop.csproj")
        );
        assert!(project_of(&root.join("tests/Two/T.cs"))
            .unwrap_err()
            .to_string()
            .contains("Several project files"));
        assert!(project_of(&root.join("Loose.cs"))
            .unwrap_err()
            .to_string()
            .contains("No project file"));
        // Without a repository the solution bounds the workspace, else the project.
        assert_eq!(workspace_root(&project), root);
        std::fs::remove_file(root.join("Shop.sln")).unwrap();
        assert_eq!(workspace_root(&project), root.join("tests/Shop.Tests"));
        // A solution beside the tests does not hide the sources it builds.
        std::fs::write(root.join("tests/Tests.sln"), "").unwrap();
        assert_eq!(workspace_root(&project), root.join("tests"));
        std::fs::create_dir(root.join(".git")).unwrap();
        assert_eq!(workspace_root(&project), root);
    }

    #[tokio::test]
    async fn startup_failure_is_a_retained_result() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("Tests.csproj");
        let (_tx, rx) = watch::channel(false);
        let result = run(
            &dir.path().join("missing-dotnet"),
            &project_selection(&project),
            rx,
            RUN_TIMEOUT,
            None,
        )
        .await;
        assert!(!result.success && !result.started);
        assert!(result.status.contains("COULD NOT START"));
        assert!(!result.output.is_empty());
    }

    #[test]
    fn the_header_shows_the_command_that_runs() {
        let project = Path::new("/src/Shop.Tests/Shop.Tests.csproj");
        let method = test(
            "Shop.Tests.CalcTests",
            "Adds",
            "/src/Shop.Tests/CalcTests.cs",
            3,
        );
        let header = header(
            &selection(project, &method, Some("a note".into()), &[]),
            false,
        );
        assert!(header.starts_with(
            ".NET test: Shop.Tests.CalcTests.Adds\nProject: /src/Shop.Tests/Shop.Tests.csproj\n"
        ));
        assert!(header.contains(
            "Command: dotnet test \"Shop.Tests.csproj\" --nologo --logger \"console;verbosity=normal\" --filter \"FullyQualifiedName=Shop.Tests.CalcTests.Adds\"\n"
        ));
        assert!(header.ends_with("Original selection: a note\n\n"));
        let all = super::header(&project_selection(project), false);
        assert!(all.starts_with(".NET test: All tests in project\n"));
        assert!(all.contains("--logger \"console;verbosity=normal\"\n"));
        assert_eq!(project_file(&lines(&all)), Some(project.to_owned()));
        // A debugged run shows what makes the test host wait.
        let debugged = super::header(&project_selection(project), true);
        assert!(debugged.contains(
            "Command: VSTEST_HOST_DEBUG=1 VSTEST_DEBUG_NOBP=1 dotnet test \"Shop.Tests.csproj\""
        ));
        assert!(debugged.contains("The debugger attaches when the test host has started"));
        assert_eq!(project_file(&lines(&debugged)), Some(project.to_owned()));
    }

    #[test]
    fn waiting_test_hosts_are_announced_once_each() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut hosts = HostWatch::new(Box::new(move |pid| tx.send(pid).unwrap()));
        let output =
            "Test run for /src/bin/Debug/net8.0/Shop.Tests.dll (.NETCoreApp,Version=v8.0)\n\
            Host debugging is enabled. Please attach debugger to testhost process to continue.\n\
            Process Id: 3279490, Name: dotnet\n\
            Waiting for debugger attach...\n\
            Process Id: 3279490, Name: dotnet\n\
              Passed Shop.Tests.CalcTests.Adds [7 ms]\n\
            The test printed Process Id: 1, Name: nothing\n\
            Process Id: 3279777, Name: dotnet\n";
        // The output arrives in pieces that end anywhere.
        let mut seen = Vec::new();
        let mut shown = 0;
        for piece in output.as_bytes().chunks(7) {
            hosts.see(piece);
            shown += piece.len();
            seen.extend(rx.try_iter());
            let announced = output[..shown].contains("Process Id: 3279490, Name: dotnet\n");
            assert_eq!(seen.contains(&3279490), announced, "after {shown} bytes");
        }
        assert_eq!(seen, [3279490, 3279777]);
        // Without a line end there is nothing to read yet, and a long line is no announcement.
        let mut hosts = HostWatch::new(Box::new(|_| panic!("not an announcement")));
        hosts.see(b"Process Id: 12");
        hosts.see(format!("{}Process Id: 99, Name: dotnet\n", "x".repeat(300)).as_bytes());
    }

    /// An xUnit project below a solution directory, in a path with spaces.
    /// `Slow` only waits while a file named `hold` exists beside the solution.
    pub(super) fn dotnet_fixture() -> tempfile::TempDir {
        let version = std::process::Command::new("dotnet")
            .arg("--version")
            .output()
            .expect("dotnet on PATH");
        let version = String::from_utf8_lossy(&version.stdout).into_owned();
        let major = version.split('.').next().unwrap().trim();
        let fixture = tempfile::Builder::new()
            .prefix("helix .NET tests ")
            .tempdir()
            .unwrap();
        let root = helix_stdx::path::canonicalize(fixture.path());
        let project = root.join("tests/Shop.Tests");
        std::fs::create_dir_all(&project).unwrap();
        for (file, contents) in [
            ("Shop.sln", String::new()),
            (
                "tests/Shop.Tests/Shop.Tests.csproj",
                format!(
                    r#"<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup>
    <TargetFramework>net{major}.0</TargetFramework>
    <ImplicitUsings>enable</ImplicitUsings>
    <IsPackable>false</IsPackable>
  </PropertyGroup>
  <ItemGroup>
    <PackageReference Include="Microsoft.NET.Test.Sdk" Version="17.14.1" />
    <PackageReference Include="xunit" Version="2.9.3" />
    <PackageReference Include="xunit.runner.visualstudio" Version="3.1.4" />
  </ItemGroup>
  <ItemGroup>
    <Using Include="Xunit" />
  </ItemGroup>
</Project>
"#
                ),
            ),
            (
                "tests/Shop.Tests/CalcTests.cs",
                r#"namespace Shop.Tests;

public class CalcTests
{
    [Fact]
    public void Adds() => Console.WriteLine("CHOSEN TEST");

    [Fact]
    public void AddsMore() => Assert.Fail("SIBLING TEST");

    [Theory]
    [InlineData(1, 2, 3)]
    [InlineData(2, 2, 5)]
    public void Table(int a, int b, int sum)
    {
        Assert.Equal(sum, a + b);
    }

    [Fact(Skip = "not available")]
    public void Skipped() { }

    public class Nested
    {
        [Fact]
        public void Inner() => Console.WriteLine("NESTED TEST");
    }
}
"#
                .to_owned(),
            ),
            (
                "tests/Shop.Tests/Collide.cs",
                r#"namespace MyShop.Tests;

public class CalcTests
{
    [Fact]
    public void Elsewhere() => Console.WriteLine("OTHER NAMESPACE");
}
"#
                .to_owned(),
            ),
            (
                "tests/Shop.Tests/SlowTests.cs",
                format!(
                    r#"namespace Shop.Tests;

public class SlowTests
{{
    [Fact]
    public void Slow()
    {{
        if (!File.Exists(@"{root}/hold")) return;
        File.WriteAllText(@"{root}/pid", Environment.ProcessId.ToString());
        Thread.Sleep(60000);
    }}
}}
"#,
                    root = root.display()
                ),
            ),
        ] {
            std::fs::write(root.join(file), contents).unwrap();
        }
        fixture
    }

    pub(super) fn fixture_project(fixture: &tempfile::TempDir) -> PathBuf {
        helix_stdx::path::canonicalize(fixture.path()).join("tests/Shop.Tests/Shop.Tests.csproj")
    }

    async fn run_dotnet(target: &DotnetTestRun) -> RunResult {
        let (_tx, rx) = watch::channel(false);
        run(Path::new("dotnet"), target, rx, RUN_TIMEOUT, None).await
    }

    #[tokio::test]
    #[ignore = "requires the .NET SDK, the C# grammar and the xunit packages (NuGet cache or network)"]
    async fn real_dotnet_methods_classes_projects_and_build_errors() {
        let fixture = dotnet_fixture();
        let project = fixture_project(&fixture);
        let source = project.with_file_name("CalcTests.cs");
        let tests = discovery::project_tests(project.parent().unwrap()).unwrap();
        let select = |name: &str| {
            let test = tests
                .iter()
                .find(|test| test.name() == name)
                .unwrap_or_else(|| panic!("{name} was not discovered"));
            selection(&project, test, None, &tests)
        };
        let report = |result: &RunResult| format!("{}\n{}", result.status, result.output);

        // An exact name: the sibling that shares its prefix stays out.
        let chosen = run_dotnet(&select("Shop.Tests.CalcTests.Adds")).await;
        assert!(chosen.success, "{}", report(&chosen));
        assert_eq!(chosen.status, "PASSED (1 passed)");
        assert!(chosen.output.contains("CHOSEN TEST"), "{}", report(&chosen));
        assert!(!chosen.output.contains("SIBLING TEST"));

        // A method is all of its data rows, and a failing row leads to its line.
        let table = run_dotnet(&select("Shop.Tests.CalcTests.Table")).await;
        assert_eq!(
            table.status,
            "FAILED (1 failed, 1 passed)",
            "{}",
            report(&table)
        );
        assert!(!table.success);
        assert!(table
            .output
            .lines()
            .filter_map(source_location)
            .any(|location| location.path == source && location.line == 15));
        assert_eq!(
            table
                .output
                .lines()
                .find_map(|line| Some(
                    RESULT.captures(line).filter(|c| &c[1] == "Failed")?[2].to_owned()
                ))
                .as_deref(),
            Some("Shop.Tests.CalcTests.Table(a: 2, b: 2, sum: 5)")
        );

        // A class is its own methods: not those of the classes nested in it,
        // and not those of a class whose longer name ends in its own.
        let class = select("Shop.Tests.CalcTests");
        assert_eq!(
            class.filter.as_deref(),
            Some("FullyQualifiedName~Shop.Tests.CalcTests.&FullyQualifiedName!~MyShop.Tests.CalcTests.")
        );
        let class = run_dotnet(&class).await;
        assert_eq!(
            class.status,
            "FAILED (2 failed, 2 passed, 1 skipped)",
            "{}",
            report(&class)
        );
        assert!(!class.output.contains("NESTED TEST"));
        assert!(!class.output.contains("OTHER NAMESPACE"));
        let nested = run_dotnet(&select("Shop.Tests.CalcTests+Nested")).await;
        assert_eq!(nested.status, "PASSED (1 passed)", "{}", report(&nested));
        assert!(nested.output.contains("NESTED TEST"));

        let skipped = run_dotnet(&select("Shop.Tests.CalcTests.Skipped")).await;
        assert_eq!(
            skipped.status,
            "SKIPPED (1 skipped)",
            "{}",
            report(&skipped)
        );
        assert!(!skipped.success);

        let mut gone = select("Shop.Tests.CalcTests.Adds");
        gone.filter = Some("FullyQualifiedName=Shop.Tests.CalcTests.Gone".into());
        let gone = run_dotnet(&gone).await;
        assert!(gone.status.starts_with("NOT RUN"), "{}", report(&gone));
        assert!(!gone.success && gone.started);

        let all = run_dotnet(&project_selection(&project)).await;
        assert_eq!(
            all.status,
            "FAILED (2 failed, 5 passed, 1 skipped)",
            "{}",
            report(&all)
        );

        let broken = project.with_file_name("Broken.cs");
        std::fs::write(&broken, "class Broken { int x = undefinedName; }\n").unwrap();
        let build = run_dotnet(&select("Shop.Tests.CalcTests.Adds")).await;
        assert!(
            build.status.starts_with("BUILD FAILED"),
            "{}",
            report(&build)
        );
        assert!(build.output.contains("undefinedName"));
        assert!(build
            .output
            .lines()
            .filter_map(source_location)
            .any(|location| location.path == broken && location.line == 0));
    }

    #[tokio::test]
    #[cfg(unix)]
    #[ignore = "requires the .NET SDK and the xunit packages; checks that cancelling stops the test host"]
    async fn real_dotnet_cancel_stops_the_test_host() {
        let fixture = dotnet_fixture();
        let project = fixture_project(&fixture);
        let root = helix_stdx::path::canonicalize(fixture.path());
        std::fs::write(root.join("hold"), "").unwrap();
        let slow = DotnetTest {
            class: "Shop.Tests.SlowTests".into(),
            method: Some("Slow".into()),
            file: project.with_file_name("SlowTests.cs"),
            line: 5,
        };
        let target = selection(&project, &slow, None, &[]);
        let (tx, rx) = watch::channel(false);
        let task =
            tokio::spawn(
                async move { run(Path::new("dotnet"), &target, rx, RUN_TIMEOUT, None).await },
            );
        let pid_path = root.join("pid");
        // The test host is a grandchild of `dotnet test`, started after the build.
        tokio::time::timeout(Duration::from_secs(180), async {
            while !pid_path.exists() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            // Written and closed in one call, but not necessarily yet.
            while std::fs::read_to_string(&pid_path).is_ok_and(|pid| pid.is_empty()) {
                tokio::time::sleep(Duration::from_millis(10)).await;
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
    }
}

#[cfg(test)]
mod debug_tests {
    use super::tests::{dotnet_fixture, fixture_project};
    use super::*;

    #[tokio::test]
    #[cfg(unix)]
    #[ignore = "requires the .NET SDK and the xunit packages; checks that a debugged run waits in the test host"]
    async fn real_dotnet_test_host_waits_for_the_debugger() {
        let fixture = dotnet_fixture();
        let project = fixture_project(&fixture);
        let adds = DotnetTest {
            class: "Shop.Tests.CalcTests".into(),
            method: Some("Adds".into()),
            file: project.with_file_name("CalcTests.cs"),
            line: 5,
        };
        let target = selection(&project, &adds, None, &[]);
        let (cancel, rx) = watch::channel(false);
        let (tx, mut hosts) = tokio::sync::mpsc::unbounded_channel();
        let attach: Attach = Box::new(move |pid| tx.send(pid).unwrap());
        let task = tokio::spawn(async move {
            run(Path::new("dotnet"), &target, rx, RUN_TIMEOUT, Some(attach)).await
        });
        let pid = tokio::time::timeout(Duration::from_secs(180), hosts.recv())
            .await
            .expect("the test host announces itself")
            .unwrap();
        // The announced process is the one that will run the tests, and it is
        // still there: nothing ran while no debugger came.
        let command = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap();
        assert!(
            String::from_utf8_lossy(&command).contains("testhost"),
            "{}",
            String::from_utf8_lossy(&command)
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(!task.is_finished());
        cancel.send(true).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.status, "CANCELLED");
        assert!(result.output.contains("Waiting for debugger attach"));
        assert!(!result.output.contains("CHOSEN TEST"), "{}", result.output);
    }
}

#[cfg(all(test, feature = "integration"))]
mod editor_tests {
    use super::super::harness::*;
    use super::tests::{dotnet_fixture, fixture_project};
    use super::*;
    use crate::application::Application;

    #[tokio::test(flavor = "multi_thread")]
    async fn commands_need_a_project_and_saved_buffers() {
        let fixture = tempfile::tempdir().unwrap();
        let root = helix_stdx::path::canonicalize(fixture.path());
        let loose = root.join("Loose.cs");
        let source = root.join("Shop.Tests/CalcTests.cs");
        std::fs::create_dir(root.join("Shop.Tests")).unwrap();
        std::fs::write(&loose, "class Loose { }\n").unwrap();
        std::fs::write(root.join("Shop.Tests/Shop.Tests.csproj"), "<Project />\n").unwrap();
        std::fs::write(&source, "class CalcTests { }\n").unwrap();
        let mut app = test_app(&loose);
        for command in [":test-package<ret>", "<space>tt", "<space>tn"] {
            keys(&mut app, command).await;
            assert!(status(&app).contains("No project file"), "{}", status(&app));
        }
        app.editor.open(&source, Action::Replace).unwrap();
        for command in ["<space>tp", "<space>tt", "<space>tn"] {
            keys(&mut app, &format!("i// unsaved<esc>{command}")).await;
            assert!(
                status(&app).contains("Save modified files"),
                "{}",
                status(&app)
            );
            keys(&mut app, "u").await;
        }
        assert!(app.editor.test_doc_id.is_none());
        assert!(app.editor.test_last_run.is_none());
        // Another language's file selects another runner, and none at all for the rest.
        let notes = root.join("notes.txt");
        std::fs::write(&notes, "notes\n").unwrap();
        app.editor.open(&notes, Action::Replace).unwrap();
        keys(&mut app, "<space>tp").await;
        assert!(status(&app).contains("Open a saved Go or C# file"));
        assert!(app.close().await.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires the .NET SDK, the C# grammar and the xunit packages; runs from the cursor to the failing line"]
    async fn cursor_run_rerun_and_navigation_from_the_output() {
        let fixture = dotnet_fixture();
        let project = fixture_project(&fixture);
        let source = project.with_file_name("CalcTests.cs");
        let mut app = test_app(&source);
        let cursor_on = |app: &mut Application, needle: &str| {
            let (view, doc) = current!(app.editor);
            let text = doc.text().to_string();
            let position = text[..text.find(needle).unwrap()].chars().count();
            doc.set_selection(view.id, helix_core::Selection::point(position));
        };

        // Outside any class there is nothing to run.
        keys(&mut app, "<space>tn").await;
        assert!(
            status(&app).contains("Cursor is not inside a test method"),
            "{}",
            status(&app)
        );
        assert!(app.editor.test_doc_id.is_none());

        cursor_on(&mut app, "Assert.Equal(sum");
        keys(&mut app, "<space>tn").await;
        let output = finished_output(&mut app).await;
        assert!(
            output.starts_with(".NET test: Shop.Tests.CalcTests.Table\n"),
            "{output}"
        );
        assert!(
            output.contains("\nFAILED (1 failed, 1 passed)\n"),
            "{output}"
        );
        // The source keeps the focus while the results arrive beside it.
        assert_eq!(doc!(app.editor).path(), Some(&source));
        let Some(TestRun::Dotnet(last)) = app.editor.test_last_run.clone() else {
            panic!("no .NET run recorded");
        };
        assert_eq!(last.project, project);
        assert_eq!(
            last.filter.as_deref(),
            Some("FullyQualifiedName=Shop.Tests.CalcTests.Table")
        );

        // A result opens the test, a frame its line, any other line its test.
        follow(&mut app, "  Failed Shop.Tests.CalcTests.Table").await;
        assert_eq!(position(&app), (source.clone(), 14));
        follow(&mut app, "CalcTests.cs:line 16").await;
        assert_eq!(position(&app), (source.clone(), 16));
        follow(&mut app, "  Error Message:").await;
        assert_eq!(position(&app), (source.clone(), 14));
        follow(&mut app, "  Passed Shop.Tests.CalcTests.Table").await;
        assert_eq!(position(&app), (source.clone(), 14));

        // On the class line the whole class runs, and says why.
        cursor_on(&mut app, "public class Nested");
        keys(&mut app, "<space>tn").await;
        let output = finished_output(&mut app).await;
        assert!(
            output.starts_with(".NET test: Shop.Tests.CalcTests+Nested\n"),
            "{output}"
        );
        assert!(output.contains("Original selection: No test method at the cursor"));
        assert!(output.contains("\nPASSED (1 passed)\n"), "{output}");

        // The last selection reruns from any buffer, the project from any of its files.
        app.editor
            .open(&project.with_file_name("SlowTests.cs"), Action::Replace)
            .unwrap();
        keys(&mut app, "<space>tl").await;
        let output = finished_output(&mut app).await;
        assert!(
            output.starts_with(".NET test: Shop.Tests.CalcTests+Nested\n"),
            "{output}"
        );
        keys(&mut app, ":test-package<ret>").await;
        let output = finished_output(&mut app).await;
        assert!(
            output.starts_with(".NET test: All tests in project\n"),
            "{output}"
        );
        assert!(
            output.contains("\nFAILED (2 failed, 5 passed, 1 skipped)\n"),
            "{output}"
        );
        assert!(app.close().await.is_empty());
    }
}
