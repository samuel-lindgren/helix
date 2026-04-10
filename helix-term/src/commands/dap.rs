use super::{Context, Editor};
use crate::{
    compositor::{self, Compositor},
    job::{Callback, Jobs},
    ui::{
        self,
        overlay::{corner_overlaid, overlaid},
        DebugOutputPanel, DebugVariables, Picker, Prompt, PromptEvent,
    },
};
use dap::{StackFrame, Thread, ThreadStates};
use helix_core::syntax::config::{DebugConfigCompletion, DebugTemplate};
use helix_core::{Selection, Transaction};
use helix_dap::{self as dap, requests::TerminateArguments};
use helix_lsp::block_on;
use helix_view::editor::Breakpoint;

use serde_json::{to_value, Value};

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail};

use helix_view::handlers::dap::{breakpoints_changed, jump_to_stack_frame, select_thread_id};

fn thread_picker(
    cx: &mut Context,
    callback_fn: impl Fn(&mut Editor, &dap::Thread) + Send + 'static,
) {
    let debugger = debugger!(cx.editor);

    let future = debugger.threads();
    dap_callback(
        cx.jobs,
        future,
        move |editor, compositor, response: dap::requests::ThreadsResponse| {
            let threads = response.threads;
            if threads.len() == 1 {
                callback_fn(editor, &threads[0]);
                return;
            }
            let debugger = debugger!(editor);

            let thread_states = debugger.thread_states.clone();
            let columns = [
                ui::PickerColumn::new("name", |item: &Thread, _| item.name.as_str().into()),
                ui::PickerColumn::new("state", |item: &Thread, thread_states: &ThreadStates| {
                    thread_states
                        .get(&item.id)
                        .map(|state| state.as_str())
                        .unwrap_or("unknown")
                        .into()
                }),
            ];
            let picker = Picker::new(
                columns,
                0,
                threads,
                thread_states,
                move |cx, thread, _action| callback_fn(cx.editor, thread),
            )
            .with_preview(move |editor, thread| {
                let frames = editor
                    .debug_adapters
                    .get_active_client()
                    .as_ref()?
                    .stack_frames
                    .get(&thread.id)?;
                let frame = frames.first()?;
                let path = frame.source.as_ref()?.path.as_ref()?.as_path();
                let pos = Some((
                    frame.line.saturating_sub(1),
                    frame.end_line.unwrap_or(frame.line).saturating_sub(1),
                ));
                Some((path.into(), pos))
            });
            compositor.push(Box::new(picker));
        },
    );
}

fn get_breakpoint_at_current_line(editor: &mut Editor) -> Option<(usize, Breakpoint)> {
    let (view, doc) = current!(editor);
    let text = doc.text().slice(..);

    let line = doc.selection(view.id).primary().cursor_line(text);
    let path = doc.path()?;
    editor.breakpoints.get(path).and_then(|breakpoints| {
        let i = breakpoints.iter().position(|b| b.line == line);
        i.map(|i| (i, breakpoints[i].clone()))
    })
}

// -- DAP

fn dap_callback<T, F>(
    jobs: &mut Jobs,
    call: impl Future<Output = helix_dap::Result<serde_json::Value>> + 'static + Send,
    callback: F,
) where
    T: for<'de> serde::Deserialize<'de> + Send + 'static,
    F: FnOnce(&mut Editor, &mut Compositor, T) + Send + 'static,
{
    let callback = Box::pin(async move {
        let json = call.await?;
        let response = serde_json::from_value(json)?;
        let call: Callback = Callback::EditorCompositor(Box::new(
            move |editor: &mut Editor, compositor: &mut Compositor| {
                callback(editor, compositor, response)
            },
        ));
        Ok(call)
    });

    jobs.callback(callback);
}

pub fn dap_start_impl(
    cx: &mut compositor::Context,
    name: Option<&str>,
    socket: Option<std::net::SocketAddr>,
    params: Option<Vec<std::borrow::Cow<str>>>,
) -> Result<(), anyhow::Error> {
    let doc = doc!(cx.editor);
    let config = doc
        .language_config()
        .and_then(|config| config.debugger.as_ref())
        .ok_or_else(|| anyhow!("No debug adapter available for language"))?
        .clone();

    cx.editor.set_status("Starting debug adapter...");
    cx.editor.debug_output_log.clear();

    let id = cx
        .editor
        .debug_adapters
        .start_client(socket, &config)
        .map_err(|e| anyhow!("Failed to start debug client: {}", e))?;

    // TODO: avoid refetching all of this... pass a config in
    let template = match name {
        Some(name) => config.templates.iter().find(|t| t.name == name),
        None => config.templates.first(),
    }
    .ok_or_else(|| anyhow!("No debug config with given name"))?;

    let mut args: HashMap<&str, Value> = if let Some(params) = params.as_ref() {
        let preprocessed_params = prepare_dap_params(template, params);
        template
            .args
            .iter()
            .map(|(k, v)| (k.as_str(), map_value(v, &preprocessed_params)))
            .collect()
    } else {
        template
            .args
            .iter()
            .map(|(k, v)| (k.as_str(), v.clone()))
            .collect()
    };

    args.insert("cwd", to_value(helix_stdx::env::current_working_dir())?);

    let args = to_value(args).unwrap();

    let request_type = template.request.clone();
    let callback = move |editor: &mut Editor, _compositor: &mut Compositor, _response: Value| {
        editor.set_status(format!("Debug {} request accepted", request_type));
    };

    let debugger = match cx.editor.debug_adapters.get_client_mut(id) {
        Some(child) => child,
        None => {
            bail!("Failed to get child debugger.");
        }
    };

    match &template.request[..] {
        "launch" => {
            let call = debugger.launch(args);
            dap_callback(cx.jobs, call, callback);
        }
        "attach" => {
            let call = debugger.attach(args);
            dap_callback(cx.jobs, call, callback);
        }
        request => bail!("Unsupported request '{}'", request),
    };

    // Open the debug output panel so the user can see startup progress.
    let open_panel = Box::pin(async {
        let call: crate::job::Callback =
            crate::job::Callback::EditorCompositor(Box::new(|_editor, compositor| {
                compositor.remove(DebugOutputPanel::ID);
                compositor.push(Box::new(corner_overlaid(DebugOutputPanel::for_startup())));
            }));
        Ok(call)
    });
    cx.jobs.callback(open_panel);

    Ok(())
}

fn prepare_dap_params(template: &DebugTemplate, params: &[std::borrow::Cow<str>]) -> Vec<String> {
    params
        .iter()
        .enumerate()
        .map(|(i, x)| {
            let mut param = x.to_string();
            if let Some(DebugConfigCompletion::Advanced(cfg)) = template.completion.get(i) {
                if matches!(cfg.completion.as_deref(), Some("filename" | "directory")) {
                    param = std::fs::canonicalize(x.as_ref())
                        .ok()
                        .and_then(|pb| pb.into_os_string().into_string().ok())
                        .unwrap_or_else(|| x.to_string());
                }
            }
            param
        })
        .collect()
}

fn map_value(value: &Value, params: &[String]) -> Value {
    match value {
        Value::String(string) => {
            let mut string = string.clone();
            for (i, x) in params.iter().enumerate() {
                let pattern = format!("{{{}}}", i);
                string = string.replace(&pattern, x);
            }
            if let Ok(integer) = string.parse::<usize>() {
                to_value(integer).unwrap()
            } else {
                to_value(string).unwrap()
            }
        }
        Value::Array(array) => Value::Array(array.iter().map(|x| map_value(x, params)).collect()),
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(k, v)| (k.clone(), map_value(v, params)))
                .collect(),
        ),

        _ => value.clone(),
    }
}

/// A Go test entry discovered by scanning `_test.go` files in a package
/// directory. Used by the `go-test-function` debug template completion.
///
/// Represents either a top-level `func TestX` (when `subtest` is `None` —
/// Delve runs every subtest under it) or a specific case within such a
/// function (`subtest = Some("…")` — Delve runs only that one).
#[derive(Clone)]
struct GoTestEntry {
    /// Bare function name, e.g. `TestDownloadArtifacts`.
    name: String,
    /// The specific subtest case, in source form (spaces preserved for
    /// display). `None` means "the whole test function".
    subtest: Option<String>,
    /// The `_test.go` file the function is declared in (basename only,
    /// for display in the picker).
    file: String,
}

/// Scan `*_test.go` files in `dir` (non-recursive) for top-level
/// `func Test…(…)` declarations plus any statically-detectable subtests
/// inside them, returning the entries sorted by test then subtest.
///
/// Uses a simple line-level scan rather than tree-sitter: Go test function
/// signatures are very standardised (`func TestX(t *testing.T)`) and
/// gofmt'd bodies let us delimit a test function by matching its opening
/// `func` line to the next line beginning with `}` at column 0. Within
/// that body we recognise two common subtest patterns:
///   1. `t.Run("literal", …)` — direct subtest declarations.
///   2. `name: "literal"` / `Name: "literal"` — the struct-field convention
///      for table-driven tests.
/// Dynamic names (`t.Run(tc.name, …)` without a matching `name:` field,
/// `fmt.Sprintf(…)` etc.) are not enumerated; the parent entry still lets
/// the user run all subtests under the function.
fn find_go_tests_in_dir(dir: &Path) -> Vec<GoTestEntry> {
    let mut tests = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return tests;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !file_name.ends_with("_test.go") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        parse_go_test_file(&content, file_name, &mut tests);
    }
    tests.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| match (&a.subtest, &b.subtest) {
                (None, None) => std::cmp::Ordering::Equal,
                // Parent entry sorts before its own subtests.
                (None, Some(_)) => std::cmp::Ordering::Less,
                (Some(_), None) => std::cmp::Ordering::Greater,
                (Some(x), Some(y)) => x.cmp(y),
            })
    });
    tests
}

/// Parse one `_test.go` file and push discovered tests + subtests into `out`.
fn parse_go_test_file(content: &str, file_name: &str, out: &mut Vec<GoTestEntry>) {
    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        if let Some(test_name) = extract_go_test_fn_name(lines[i]) {
            // Body runs from i+1 until the next line beginning with `}` at
            // column 0 (gofmt guarantees this is the function's closing
            // brace — nested `}` are always indented).
            let body_end = ((i + 1)..lines.len())
                .find(|&j| lines[j].starts_with('}'))
                .unwrap_or(lines.len());

            let mut subtests: Vec<String> = Vec::new();
            let mut seen: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            for body_line in &lines[(i + 1)..body_end] {
                if let Some(sub) = extract_go_subtest_name(body_line) {
                    if seen.insert(sub.clone()) {
                        subtests.push(sub);
                    }
                }
            }

            out.push(GoTestEntry {
                name: test_name.clone(),
                subtest: None,
                file: file_name.to_string(),
            });
            for sub in subtests {
                out.push(GoTestEntry {
                    name: test_name.clone(),
                    subtest: Some(sub),
                    file: file_name.to_string(),
                });
            }
            // Skip past the body so we don't re-scan its lines.
            i = body_end + 1;
        } else {
            i += 1;
        }
    }
}

/// Recognise a top-level `func TestXxx(...)` line and return the bare
/// function name. Returns `None` for anything else (including methods,
/// Benchmarks, Fuzz tests, or bare `Test`).
fn extract_go_test_fn_name(line: &str) -> Option<String> {
    let after_func = line.strip_prefix("func")?;
    // Require at least one space between `func` and the name so we don't
    // match identifiers like `functional`.
    if !after_func.starts_with(|c: char| c.is_ascii_whitespace()) {
        return None;
    }
    let rest = after_func.trim_start();
    // Only `Test…` — Benchmarks and Fuzz tests need different Delve flags
    // and would clutter the picker for the common case.
    if !rest.starts_with("Test") {
        return None;
    }
    let end = rest
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .unwrap_or(rest.len());
    // Reject bare `Test` — a real test function is `TestFoo`.
    if end <= 4 {
        return None;
    }
    if !rest[end..].trim_start().starts_with('(') {
        return None;
    }
    Some(rest[..end].to_string())
}

/// Recognise a subtest-name literal on a single line. Handles two forms:
///   `t.Run("name", …)`           — direct call
///   `name: "name",` / `Name: …`  — table-driven struct field
///
/// Returns `None` if the line doesn't match, or if the quoted name
/// contains characters (`\`, `/`, raw quote) that make it awkward to
/// round-trip through Go's `-test.run` regex — in that case the parent
/// entry still covers it.
fn extract_go_subtest_name(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    if let Some(after) = trimmed.strip_prefix("t.Run(\"") {
        return take_simple_go_string(after);
    }
    for prefix in ["name:", "Name:"] {
        if let Some(after) = trimmed.strip_prefix(prefix) {
            let after = after.trim_start();
            if let Some(after) = after.strip_prefix('"') {
                return take_simple_go_string(after);
            }
        }
    }
    None
}

/// Read characters from `s` up to the first closing `"`. Rejects strings
/// containing escape sequences (`\`) or `/` since both complicate the
/// `-test.run` regex (escape decoding + the `/` subtest separator).
fn take_simple_go_string(s: &str) -> Option<String> {
    let end = s.find('"')?;
    let content = &s[..end];
    if content.contains('\\') || content.contains('/') {
        return None;
    }
    Some(content.to_string())
}

/// Build the value for Delve's `-test.run` flag for a given entry.
///
/// Parent entry → `^TestFoo$`. Subtest entry → `^TestFoo$/^case_name$`,
/// with the subtest name rewritten the way Go's test runner does it
/// (ASCII whitespace → `_`) and then regex-escaped so punctuation inside
/// the name matches literally.
fn go_test_run_regex(entry: &GoTestEntry) -> String {
    let root = format!("^{}$", entry.name);
    match &entry.subtest {
        None => root,
        Some(sub) => {
            // Go's testing package rewrites spaces in subtest names to
            // underscores when forming the fully-qualified path
            // (`TestFoo/case_name`), so we do the same here.
            let normalized: String = sub
                .chars()
                .map(|c| if c.is_ascii_whitespace() { '_' } else { c })
                .collect();
            let escaped = regex_escape(&normalized);
            format!("{root}/^{escaped}$")
        }
    }
}

/// Escape RE2 regex metacharacters. Also escapes `/`, which isn't a
/// regex special but IS the subtest path separator in Go's `-test.run`.
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(
            c,
            '.' | '*'
                | '+'
                | '?'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '|'
                | '^'
                | '$'
                | '\\'
                | '/'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Build the picker shown when a debug template uses the `go-test-function`
/// completion type. On selection, the picker hands two params to
/// `dap_start_impl`: the package directory (for `program`) and the
/// anchored test-name regex (for `-test.run`).
///
/// The picker lists each top-level test once, followed by any statically
/// detected subtests from that test (see [`find_go_tests_in_dir`]). The
/// user can select the parent to run everything or pick a specific case.
fn build_go_test_picker(
    config_name: String,
    pkg_dir: PathBuf,
    tests: Vec<GoTestEntry>,
) -> Picker<GoTestEntry, ()> {
    let columns = [
        ui::PickerColumn::new("test", |item: &GoTestEntry, _| item.name.as_str().into()),
        ui::PickerColumn::new("case", |item: &GoTestEntry, _| {
            item.subtest.as_deref().unwrap_or("").into()
        }),
        ui::PickerColumn::new("file", |item: &GoTestEntry, _| item.file.as_str().into()),
    ];
    let pkg_dir_str = pkg_dir.to_string_lossy().into_owned();
    Picker::new(columns, 0, tests, (), move |cx, entry, _action| {
        // `^TestName$` anchors the parent regex so sibling tests sharing
        // a prefix (e.g. TestFoo vs TestFooBar) aren't picked up. For
        // subtests we additionally append `/^case_name$` (see
        // `go_test_run_regex`).
        let params: Vec<std::borrow::Cow<str>> = vec![
            pkg_dir_str.clone().into(),
            go_test_run_regex(entry).into(),
        ];
        if let Err(err) = dap_start_impl(cx, Some(&config_name), None, Some(params)) {
            cx.editor.set_error(err.to_string());
        }
    })
}

pub fn dap_launch(cx: &mut Context) {
    // TODO: Now that we support multiple Clients, we could run multiple debuggers at once but for now keep this as is
    if cx.editor.debug_adapters.get_active_client().is_some() {
        cx.editor.set_error("Debugger is already running");
        return;
    }

    let doc = doc!(cx.editor);

    let config = match doc
        .language_config()
        .and_then(|config| config.debugger.as_ref())
    {
        Some(c) => c,
        None => {
            cx.editor
                .set_error("No debug adapter available for language");
            return;
        }
    };

    let templates = config.templates.clone();

    let columns = [ui::PickerColumn::new(
        "template",
        |item: &DebugTemplate, _| item.name.as_str().into(),
    )];

    cx.push_layer(Box::new(overlaid(Picker::new(
        columns,
        0,
        templates,
        (),
        |cx, template, _action| {
            // Detect the custom `go-test-function` completion type and route
            // to a test-name picker instead of the generic parameter prompt.
            let first_completion_kind = template.completion.first().and_then(|c| match c {
                DebugConfigCompletion::Advanced(cfg) => cfg.completion.as_deref(),
                _ => None,
            });
            if first_completion_kind == Some("go-test-function") {
                let name = template.name.clone();
                let callback = Box::pin(async move {
                    let call: Callback =
                        Callback::EditorCompositor(Box::new(move |editor, compositor| {
                            let pkg_dir = doc!(editor)
                                .path()
                                .and_then(|p| p.parent().map(Path::to_path_buf))
                                .unwrap_or_else(helix_stdx::env::current_working_dir);
                            let tests = find_go_tests_in_dir(&pkg_dir);
                            if tests.is_empty() {
                                editor.set_error(format!(
                                    "No Go tests found in {}",
                                    pkg_dir.display()
                                ));
                                return;
                            }
                            let picker = build_go_test_picker(name, pkg_dir, tests);
                            compositor.push(Box::new(overlaid(picker)));
                        }));
                    Ok(call)
                });
                cx.jobs.callback(callback);
                return;
            }

            if template.completion.is_empty() {
                if let Err(err) = dap_start_impl(cx, Some(&template.name), None, None) {
                    cx.editor.set_error(err.to_string());
                }
            } else {
                let completions = template.completion.clone();
                let name = template.name.clone();
                let callback = Box::pin(async move {
                    let call: Callback =
                        Callback::EditorCompositor(Box::new(move |_editor, compositor| {
                            let prompt = debug_parameter_prompt(completions, name, Vec::new());
                            compositor.push(Box::new(prompt));
                        }));
                    Ok(call)
                });
                cx.jobs.callback(callback);
            }
        },
    ))));
}

pub fn dap_restart(cx: &mut Context) {
    let debugger = match cx.editor.debug_adapters.get_active_client() {
        Some(debugger) => debugger,
        None => {
            cx.editor.set_error("Debugger is not running");
            return;
        }
    };
    if !debugger
        .capabilities()
        .supports_restart_request
        .unwrap_or(false)
    {
        cx.editor
            .set_error("Debugger does not support session restarts");
        return;
    }
    if debugger.starting_request_args().is_none() {
        cx.editor
            .set_error("No arguments found with which to restart the sessions");
        return;
    }

    dap_callback(
        cx.jobs,
        debugger.restart(),
        |editor, _compositor, _resp: ()| editor.set_status("Debugging session restarted"),
    );
}

fn debug_parameter_prompt(
    completions: Vec<DebugConfigCompletion>,
    config_name: String,
    mut params: Vec<String>,
) -> Prompt {
    let completion = completions.get(params.len()).unwrap();
    let field_type = if let DebugConfigCompletion::Advanced(cfg) = completion {
        cfg.completion.as_deref().unwrap_or("")
    } else {
        ""
    };
    let name = match completion {
        DebugConfigCompletion::Advanced(cfg) => cfg.name.as_deref().unwrap_or(field_type),
        DebugConfigCompletion::Named(name) => name.as_str(),
    };
    let default_val = match completion {
        DebugConfigCompletion::Advanced(cfg) => cfg.default.as_deref().unwrap_or(""),
        _ => "",
    }
    .to_owned();

    let completer = match field_type {
        "filename" => |editor: &Editor, input: &str| {
            ui::completers::filename_with_git_ignore(editor, input, false)
        },
        "directory" => |editor: &Editor, input: &str| {
            ui::completers::directory_with_git_ignore(editor, input, false)
        },
        _ => ui::completers::none,
    };

    Prompt::new(
        format!("{}: ", name).into(),
        None,
        completer,
        move |cx, input: &str, event: PromptEvent| {
            if event != PromptEvent::Validate {
                return;
            }

            let mut value = input.to_owned();
            if value.is_empty() {
                value = default_val.clone();
            }
            params.push(value);

            if params.len() < completions.len() {
                let completions = completions.clone();
                let config_name = config_name.clone();
                let params = params.clone();
                let callback = Box::pin(async move {
                    let call: Callback =
                        Callback::EditorCompositor(Box::new(move |_editor, compositor| {
                            let prompt = debug_parameter_prompt(completions, config_name, params);
                            compositor.push(Box::new(prompt));
                        }));
                    Ok(call)
                });
                cx.jobs.callback(callback);
            } else if let Err(err) = dap_start_impl(
                cx,
                Some(&config_name),
                None,
                Some(params.iter().map(|x| x.into()).collect()),
            ) {
                cx.editor.set_error(err.to_string());
            }
        },
    )
}

pub fn dap_toggle_breakpoint(cx: &mut Context) {
    let (view, doc) = current!(cx.editor);
    let path = match doc.path() {
        Some(path) => path.clone(),
        None => {
            cx.editor
                .set_error("Can't set breakpoint: document has no path");
            return;
        }
    };
    let text = doc.text().slice(..);
    let line = doc.selection(view.id).primary().cursor_line(text);
    dap_toggle_breakpoint_impl(cx, path, line);
}

pub fn dap_toggle_breakpoint_impl(cx: &mut Context, path: PathBuf, line: usize) {
    // TODO: need to map breakpoints over edits and update them?
    // we shouldn't really allow editing while debug is running though

    let breakpoints = cx.editor.breakpoints.entry(path.clone()).or_default();
    // TODO: always keep breakpoints sorted and use binary search to determine insertion point
    if let Some(pos) = breakpoints
        .iter()
        .position(|breakpoint| breakpoint.line == line)
    {
        breakpoints.remove(pos);
    } else {
        breakpoints.push(Breakpoint {
            line,
            ..Default::default()
        });
    }

    let debugger = debugger!(cx.editor);

    if let Err(e) = breakpoints_changed(debugger, path, breakpoints) {
        cx.editor
            .set_error(format!("Failed to set breakpoints: {}", e));
    }
}

pub fn dap_continue(cx: &mut Context) {
    let debugger = debugger!(cx.editor);

    if let Some(thread_id) = debugger.thread_id {
        let request = debugger.continue_thread(thread_id);

        dap_callback(
            cx.jobs,
            request,
            |editor, _compositor, _response: dap::requests::ContinueResponse| {
                debugger!(editor).resume_application();
            },
        );
    } else {
        cx.editor
            .set_error("Currently active thread is not stopped. Switch the thread.");
    }
}

pub fn dap_pause(cx: &mut Context) {
    thread_picker(cx, |editor, thread| {
        let debugger = debugger!(editor);
        let request = debugger.pause(thread.id);
        // NOTE: we don't need to set active thread id here because DAP will emit a "stopped" event
        if let Err(e) = block_on(request) {
            editor.set_error(format!("Failed to pause: {}", e));
        }
    })
}

pub fn dap_step_in(cx: &mut Context) {
    let debugger = debugger!(cx.editor);

    if let Some(thread_id) = debugger.thread_id {
        let request = debugger.step_in(thread_id);

        dap_callback(cx.jobs, request, |editor, _compositor, _response: ()| {
            debugger!(editor).resume_application();
        });
    } else {
        cx.editor
            .set_error("Currently active thread is not stopped. Switch the thread.");
    }
}

pub fn dap_step_out(cx: &mut Context) {
    let debugger = debugger!(cx.editor);

    if let Some(thread_id) = debugger.thread_id {
        let request = debugger.step_out(thread_id);
        dap_callback(cx.jobs, request, |editor, _compositor, _response: ()| {
            debugger!(editor).resume_application();
        });
    } else {
        cx.editor
            .set_error("Currently active thread is not stopped. Switch the thread.");
    }
}

pub fn dap_next(cx: &mut Context) {
    let debugger = debugger!(cx.editor);

    if let Some(thread_id) = debugger.thread_id {
        let request = debugger.next(thread_id);
        dap_callback(cx.jobs, request, |editor, _compositor, _response: ()| {
            debugger!(editor).resume_application();
        });
    } else {
        cx.editor
            .set_error("Currently active thread is not stopped. Switch the thread.");
    }
}

pub fn dap_variables(cx: &mut Context) {
    let debugger = debugger!(cx.editor);

    if debugger.thread_id.is_none() {
        cx.editor
            .set_status("Cannot access variables while target is running.");
        return;
    }
    let (frame, thread_id) = match (debugger.active_frame, debugger.thread_id) {
        (Some(frame), Some(thread_id)) => (frame, thread_id),
        _ => {
            cx.editor
                .set_status("Cannot find current stack frame to access variables.");
            return;
        }
    };

    let thread_frame = match debugger.stack_frames.get(&thread_id) {
        Some(thread_frame) => thread_frame,
        None => {
            cx.editor
                .set_error(format!("Failed to get stack frame for thread: {thread_id}"));
            return;
        }
    };
    let stack_frame = match thread_frame.get(frame) {
        Some(stack_frame) => stack_frame,
        None => {
            cx.editor.set_error(format!(
                "Failed to get stack frame for thread {thread_id} and frame {frame}."
            ));
            return;
        }
    };

    let frame_id = stack_frame.id;
    let scopes = match block_on(debugger.scopes(frame_id)) {
        Ok(s) => s,
        Err(e) => {
            cx.editor.set_error(format!("Failed to get scopes: {}", e));
            return;
        }
    };

    let debugger_id = debugger.id();
    let mut variables = DebugVariables::new(debugger_id, scopes, Some(frame_id));
    variables.build_watch_scope(cx.editor);
    variables.expand_initial(cx.editor);

    cx.replace_or_push_layer(DebugVariables::ID, overlaid(variables));
}

pub fn dap_terminate(cx: &mut Context) {
    cx.editor.set_status("Terminating debug session...");
    let debugger = debugger!(cx.editor);

    if debugger
        .caps
        .as_ref()
        .is_some_and(|c| c.supports_terminate_request.unwrap_or_default())
    {
        let terminate_arguments = Some(TerminateArguments {
            restart: Some(false),
        });

        let request = debugger.terminate(terminate_arguments);
        dap_callback(cx.jobs, request, |editor, _compositor, _response: ()| {
            // editor.set_error(format!("Failed to disconnect: {}", e));
            editor.debug_adapters.unset_active_client();
        });
    } else {
        cx.editor.debug_adapters.unset_active_client();
    }
}

pub fn dap_eval_prompt(cx: &mut Context) {
    let debugger = debugger!(cx.editor);

    if debugger.thread_id.is_none() {
        cx.editor
            .set_status("Cannot evaluate while target is running.");
        return;
    }
    let (frame, thread_id) = match (debugger.active_frame, debugger.thread_id) {
        (Some(frame), Some(thread_id)) => (frame, thread_id),
        _ => {
            cx.editor
                .set_status("Cannot find current stack frame to evaluate.");
            return;
        }
    };

    let frame_id = debugger.stack_frames[&thread_id][frame].id;

    // Pre-fill with current selection if it spans multiple characters.
    let (view, doc) = current!(cx.editor);
    let text = doc.text().slice(..);
    let primary = doc.selection(view.id).primary();
    let prefill = if primary.len() > 1 {
        primary.fragment(text).to_string()
    } else {
        String::new()
    };

    let callback = Box::pin(async move {
        let call: Callback = Callback::EditorCompositor(Box::new(move |editor, compositor| {
            let mut prompt = Prompt::new(
                "eval:".into(),
                None,
                ui::completers::none,
                move |cx, input: &str, event: PromptEvent| {
                    if event != PromptEvent::Validate {
                        return;
                    }
                    if input.is_empty() {
                        return;
                    }

                    let debugger = debugger!(cx.editor);
                    match block_on(debugger.eval(input.to_string(), Some(frame_id))) {
                        Ok(resp) => {
                            let expr = input.to_string();
                            let text = if resp.result.is_empty() {
                                format!("{} = (empty)", expr)
                            } else {
                                let header = resp
                                    .ty
                                    .as_ref()
                                    .map(|t| format!("{}: {}", expr, t))
                                    .unwrap_or_else(|| expr.clone());
                                format!(
                                    "{}\n\n{}",
                                    header,
                                    format_eval_value(&resp.result)
                                )
                            };
                            show_eval_result_in_buffer(cx.editor, text);
                        }
                        Err(e) => cx.editor.set_error(format!("Eval failed: {}", e)),
                    }
                },
            );
            if !prefill.is_empty() {
                prompt.insert_str(&prefill, editor);
            }
            compositor.push(Box::new(prompt));
        }));
        Ok(call)
    });
    cx.jobs.callback(callback);
}

pub fn dap_eval_selection(cx: &mut Context) {
    let debugger = debugger!(cx.editor);

    if debugger.thread_id.is_none() {
        cx.editor
            .set_status("Cannot evaluate while target is running.");
        return;
    }
    let (frame, thread_id) = match (debugger.active_frame, debugger.thread_id) {
        (Some(frame), Some(thread_id)) => (frame, thread_id),
        _ => {
            cx.editor
                .set_status("Cannot find current stack frame to evaluate.");
            return;
        }
    };

    let frame_id = debugger.stack_frames[&thread_id][frame].id;

    let (view, doc) = current!(cx.editor);
    let text = doc.text().slice(..);
    let primary = doc.selection(view.id).primary();

    let expression: String = if primary.len() > 1 {
        primary.fragment(text).into()
    } else {
        use helix_core::textobject::{textobject_word, TextObject};
        textobject_word(text, primary, TextObject::Inside, 1, false)
            .fragment(text)
            .into()
    };

    if expression.trim().is_empty() {
        cx.editor.set_status("No expression to evaluate.");
        return;
    }

    let debugger = debugger!(cx.editor);
    match block_on(debugger.eval(expression.clone(), Some(frame_id))) {
        Ok(resp) => {
            let text = if resp.result.is_empty() {
                format!("{} = (empty)", expression)
            } else {
                let header = resp
                    .ty
                    .as_ref()
                    .map(|t| format!("{}: {}", expression, t))
                    .unwrap_or_else(|| expression.clone());
                format!("{}\n\n{}", header, format_eval_value(&resp.result))
            };
            show_eval_result_in_buffer(cx.editor, text);
        }
        Err(e) => cx
            .editor
            .set_error(format!("Eval '{}': {}", expression, e)),
    }
}

/// Show the evaluation result in a dedicated `[dap-eval]` scratch buffer so
/// the user can navigate, search, and yank it like any other text file.
///
/// Reuses a single per-editor buffer (tracked via `Editor::debug_eval_doc_id`)
/// across invocations, acting like a dedicated watch window:
///
/// * If the buffer is already open in some view, focus that view and replace
///   its contents in place.
/// * If the buffer still exists but no view shows it (e.g. the user closed the
///   split), re-open it in a new vertical split and replace its contents.
/// * If the buffer was closed entirely, create a fresh one in a vertical split.
fn show_eval_result_in_buffer(editor: &mut Editor, content: String) {
    use helix_view::editor::Action;

    // Is there a live eval buffer from a previous invocation?
    let existing_id = match editor.debug_eval_doc_id {
        Some(id) if editor.documents.contains_key(&id) => Some(id),
        _ => None,
    };

    match existing_id {
        Some(id) => {
            // Reuse the existing eval buffer. Focus any view already showing it,
            // otherwise re-open it as a vertical split.
            let visible_view_id = editor
                .tree
                .traverse()
                .find(|(_, view)| view.doc == id)
                .map(|(view_id, _)| view_id);

            match visible_view_id {
                Some(view_id) => editor.focus(view_id),
                None => editor.switch(id, Action::VerticalSplit),
            }

            // Replace the entire buffer contents with the new result.
            let doc = doc_mut!(editor, &id);
            let view = view_mut!(editor);
            let old_len = doc.text().len_chars();
            let transaction = Transaction::change(
                doc.text(),
                std::iter::once((0, old_len, Some(content.into()))),
            )
            .with_selection(Selection::point(0));
            doc.apply(&transaction, view.id);
            // Commit and mark clean so the statusline doesn't show `[+]` and
            // `:q` doesn't prompt. `append_changes_to_history` clears pending
            // `doc.changes`; `reset_modified` then aligns the saved revision.
            doc.append_changes_to_history(view);
            doc.reset_modified();
        }
        None => {
            // First eval this session (or the previous buffer was closed).
            // Open a fresh scratch buffer in a vertical split and remember it.
            let doc_id = editor.new_file(Action::VerticalSplit);
            editor.debug_eval_doc_id = Some(doc_id);
            let doc = doc_mut!(editor, &doc_id);
            doc.set_virtual_name(Some("[dap-eval]".to_string()));
            let view = view_mut!(editor);
            doc.ensure_view_init(view.id);
            let transaction = Transaction::insert(
                doc.text(),
                doc.selection(view.id),
                content.into(),
            )
            .with_selection(Selection::point(0));
            doc.apply(&transaction, view.id);
            doc.append_changes_to_history(view);
            doc.reset_modified();
        }
    }
}

/// Format a DAP eval result for display: break struct fields onto separate lines.
fn format_eval_value(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut depth: i32 = 0;
    let mut chars = value.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '{' => {
                depth += 1;
                result.push(ch);
                result.push('\n');
                for _ in 0..depth {
                    result.push_str("  ");
                }
            }
            '}' => {
                depth -= 1;
                result.push('\n');
                for _ in 0..depth {
                    result.push_str("  ");
                }
                result.push(ch);
            }
            ',' => {
                result.push(ch);
                // Newline after comma at struct level, but not inside strings.
                if depth > 0 {
                    result.push('\n');
                    for _ in 0..depth {
                        result.push_str("  ");
                    }
                    // Skip space after comma.
                    if chars.peek() == Some(&' ') {
                        chars.next();
                    }
                }
            }
            _ => result.push(ch),
        }
    }
    result
}

pub fn dap_run_to_cursor(cx: &mut Context) {
    let (view, doc) = current!(cx.editor);
    let path = match doc.path() {
        Some(path) => path.clone(),
        None => {
            cx.editor
                .set_error("Can't run to cursor: document has no path");
            return;
        }
    };
    let text = doc.text().slice(..);
    let line = doc.selection(view.id).primary().cursor_line(text);

    let debugger = debugger!(cx.editor);

    if debugger.thread_id.is_none() {
        cx.editor
            .set_error("Currently active thread is not stopped. Switch the thread.");
        return;
    }
    let thread_id = debugger.thread_id.unwrap();

    // Add a temporary breakpoint at the cursor line (unless one already exists).
    let breakpoints = cx.editor.breakpoints.entry(path.clone()).or_default();
    let already_has = breakpoints.iter().any(|bp| bp.line == line);
    if !already_has {
        breakpoints.push(Breakpoint {
            line,
            temporary: true,
            ..Default::default()
        });
    }

    let debugger = debugger!(cx.editor);
    if let Some(breakpoints) = cx.editor.breakpoints.get_mut(&path) {
        if let Err(e) = breakpoints_changed(debugger, path, breakpoints) {
            cx.editor
                .set_error(format!("Failed to set breakpoints: {}", e));
            return;
        }
    }

    // Continue execution — the Stopped handler will clean up temporary breakpoints.
    let debugger = debugger!(cx.editor);
    let request = debugger.continue_thread(thread_id);
    dap_callback(
        cx.jobs,
        request,
        |editor, _compositor, _response: dap::requests::ContinueResponse| {
            debugger!(editor).resume_application();
        },
    );
}

pub fn dap_enable_exceptions(cx: &mut Context) {
    let debugger = debugger!(cx.editor);

    let filters = match &debugger.capabilities().exception_breakpoint_filters {
        Some(filters) => filters.iter().map(|f| f.filter.clone()).collect(),
        None => return,
    };

    let request = debugger.set_exception_breakpoints(filters);

    dap_callback(
        cx.jobs,
        request,
        |_editor, _compositor, _response: dap::requests::SetExceptionBreakpointsResponse| {
            // editor.set_error(format!("Failed to set up exception breakpoints: {}", e));
        },
    )
}

pub fn dap_disable_exceptions(cx: &mut Context) {
    let debugger = debugger!(cx.editor);

    let request = debugger.set_exception_breakpoints(Vec::new());

    dap_callback(
        cx.jobs,
        request,
        |_editor, _compositor, _response: dap::requests::SetExceptionBreakpointsResponse| {
            // editor.set_error(format!("Failed to set up exception breakpoints: {}", e));
        },
    )
}

// TODO: both edit condition and edit log need to be stable: we might get new breakpoints from the debugger which can change offsets
pub fn dap_edit_condition(cx: &mut Context) {
    if let Some((pos, breakpoint)) = get_breakpoint_at_current_line(cx.editor) {
        let path = match doc!(cx.editor).path() {
            Some(path) => path.clone(),
            None => return,
        };
        let callback = Box::pin(async move {
            let call: Callback = Callback::EditorCompositor(Box::new(move |editor, compositor| {
                let mut prompt = Prompt::new(
                    "condition:".into(),
                    None,
                    ui::completers::none,
                    move |cx, input: &str, event: PromptEvent| {
                        if event != PromptEvent::Validate {
                            return;
                        }

                        let breakpoints = &mut cx.editor.breakpoints.get_mut(&path).unwrap();
                        breakpoints[pos].condition = match input {
                            "" => None,
                            input => Some(input.to_owned()),
                        };

                        let debugger = debugger!(cx.editor);

                        if let Err(e) = breakpoints_changed(debugger, path.clone(), breakpoints) {
                            cx.editor
                                .set_error(format!("Failed to set breakpoints: {}", e));
                        }
                    },
                );
                if let Some(condition) = breakpoint.condition {
                    prompt.insert_str(&condition, editor)
                }
                compositor.push(Box::new(prompt));
            }));
            Ok(call)
        });
        cx.jobs.callback(callback);
    }
}

pub fn dap_edit_log(cx: &mut Context) {
    if let Some((pos, breakpoint)) = get_breakpoint_at_current_line(cx.editor) {
        let path = match doc!(cx.editor).path() {
            Some(path) => path.clone(),
            None => return,
        };
        let callback = Box::pin(async move {
            let call: Callback = Callback::EditorCompositor(Box::new(move |editor, compositor| {
                let mut prompt = Prompt::new(
                    "log-message:".into(),
                    None,
                    ui::completers::none,
                    move |cx, input: &str, event: PromptEvent| {
                        if event != PromptEvent::Validate {
                            return;
                        }

                        let breakpoints = &mut cx.editor.breakpoints.get_mut(&path).unwrap();
                        breakpoints[pos].log_message = match input {
                            "" => None,
                            input => Some(input.to_owned()),
                        };

                        let debugger = debugger!(cx.editor);
                        if let Err(e) = breakpoints_changed(debugger, path.clone(), breakpoints) {
                            cx.editor
                                .set_error(format!("Failed to set breakpoints: {}", e));
                        }
                    },
                );
                if let Some(log_message) = breakpoint.log_message {
                    prompt.insert_str(&log_message, editor);
                }
                compositor.push(Box::new(prompt));
            }));
            Ok(call)
        });
        cx.jobs.callback(callback);
    }
}

pub fn dap_switch_thread(cx: &mut Context) {
    thread_picker(cx, |editor, thread| {
        block_on(select_thread_id(editor, thread.id, true));
    })
}
pub fn dap_switch_stack_frame(cx: &mut Context) {
    let debugger = debugger!(cx.editor);

    let thread_id = match debugger.thread_id {
        Some(thread_id) => thread_id,
        None => {
            cx.editor.set_error("No thread is currently active");
            return;
        }
    };

    let frames = debugger.stack_frames[&thread_id].clone();

    let columns = [ui::PickerColumn::new("frame", |item: &StackFrame, _| {
        item.name.as_str().into() // TODO: include thread_states in the label
    })];
    let picker = Picker::new(columns, 0, frames, (), move |cx, frame, _action| {
        let debugger = debugger!(cx.editor);
        // TODO: this should be simpler to find
        let pos = debugger.stack_frames[&thread_id]
            .iter()
            .position(|f| f.id == frame.id);
        debugger.active_frame = pos;

        let frame = debugger.stack_frames[&thread_id]
            .get(pos.unwrap_or(0))
            .cloned();
        if let Some(frame) = &frame {
            jump_to_stack_frame(cx.editor, frame);
        }
    })
    .with_preview(move |_editor, frame| {
        frame
            .source
            .as_ref()
            .and_then(|source| source.path.as_ref())
            .map(|path| {
                (
                    path.as_path().into(),
                    Some((
                        frame.line.saturating_sub(1),
                        frame.end_line.unwrap_or(frame.line).saturating_sub(1),
                    )),
                )
            })
    });
    cx.push_layer(Box::new(picker))
}

pub fn dap_breakpoint_picker(cx: &mut Context) {
    #[derive(Clone)]
    struct BreakpointItem {
        path: PathBuf,
        line: usize,
        condition: Option<String>,
        log_message: Option<String>,
        verified: bool,
    }

    let items: Vec<BreakpointItem> = cx
        .editor
        .breakpoints
        .iter()
        .flat_map(|(path, breakpoints)| {
            breakpoints.iter().map(move |bp| BreakpointItem {
                path: path.clone(),
                line: bp.line,
                condition: bp.condition.clone(),
                log_message: bp.log_message.clone(),
                verified: bp.verified,
            })
        })
        .collect();

    if items.is_empty() {
        cx.editor.set_status("No breakpoints set");
        return;
    }

    let columns = [
        ui::PickerColumn::new("file", |item: &BreakpointItem, _| {
            let name = item
                .path
                .file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default();
            format!("{}:{}", name, item.line + 1).into()
        }),
        ui::PickerColumn::new("info", |item: &BreakpointItem, _| {
            let mut parts = Vec::new();
            if let Some(cond) = &item.condition {
                parts.push(format!("if {}", cond));
            }
            if let Some(log) = &item.log_message {
                parts.push(format!("log: {}", log));
            }
            if !item.verified {
                parts.push("(unverified)".to_string());
            }
            parts.join(" ").into()
        }),
    ];

    let picker = Picker::new(columns, 0, items, (), |cx, item, _action| {
        let path = item.path.clone();
        let line = item.line;
        if let Err(e) = cx.editor.open(&path, helix_view::editor::Action::Replace) {
            cx.editor.set_error(format!("Failed to open file: {}", e));
            return;
        }
        let (view, doc) = current!(cx.editor);
        let pos = doc.text().line_to_char(line);
        doc.set_selection(view.id, Selection::point(pos));
    })
    .with_preview(|_editor, item| {
        Some((
            item.path.as_path().into(),
            Some((item.line, item.line)),
        ))
    });
    cx.push_layer(Box::new(picker))
}

pub fn dap_add_watch(cx: &mut Context) {
    // Pre-fill with current selection text if any.
    let (view, doc) = current!(cx.editor);
    let text = doc.text().slice(..);
    let primary = doc.selection(view.id).primary();
    let prefill = if primary.len() > 1 {
        primary.fragment(text).to_string()
    } else {
        use helix_core::textobject::{textobject_word, TextObject};
        let word = textobject_word(text, primary, TextObject::Inside, 1, false);
        let fragment = word.fragment(text);
        if fragment.trim().is_empty() {
            String::new()
        } else {
            fragment.to_string()
        }
    };

    let callback = Box::pin(async move {
        let call: Callback = Callback::EditorCompositor(Box::new(move |editor, compositor| {
            let mut prompt = Prompt::new(
                "watch:".into(),
                None,
                ui::completers::none,
                move |cx, input: &str, event: PromptEvent| {
                    if event != PromptEvent::Validate {
                        return;
                    }
                    let expr = input.trim().to_string();
                    if expr.is_empty() {
                        return;
                    }
                    if !cx.editor.watch_expressions.contains(&expr) {
                        cx.editor.watch_expressions.push(expr.clone());
                        cx.editor
                            .set_status(format!("Added watch: {}", expr));
                    } else {
                        cx.editor
                            .set_status(format!("Already watching: {}", expr));
                    }
                },
            );
            if !prefill.is_empty() {
                prompt.insert_str(&prefill, editor);
            }
            compositor.push(Box::new(prompt));
        }));
        Ok(call)
    });
    cx.jobs.callback(callback);
}

pub fn dap_remove_watch(cx: &mut Context) {
    let expressions = cx.editor.watch_expressions.clone();
    if expressions.is_empty() {
        cx.editor.set_status("No watch expressions to remove");
        return;
    }

    let columns = [ui::PickerColumn::new("expression", |item: &String, _| {
        item.as_str().into()
    })];
    let picker = Picker::new(columns, 0, expressions, (), |cx, expr, _action| {
        cx.editor.watch_expressions.retain(|e| e != expr);
        cx.editor
            .set_status(format!("Removed watch: {}", expr));
    });
    cx.push_layer(Box::new(picker));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collect only (name, subtest) from parse results so assertions don't
    /// also have to track the file name.
    fn parse(src: &str) -> Vec<(String, Option<String>)> {
        let mut out = Vec::new();
        parse_go_test_file(src, "foo_test.go", &mut out);
        out.into_iter().map(|e| (e.name, e.subtest)).collect()
    }

    #[test]
    fn finds_bare_test_function() {
        let src = "\
package foo

func TestSimple(t *testing.T) {
    t.Fatal(\"nope\")
}
";
        assert_eq!(parse(src), vec![("TestSimple".into(), None)]);
    }

    #[test]
    fn finds_direct_t_run_subtests() {
        let src = "\
package foo

func TestDirect(t *testing.T) {
    t.Run(\"first case\", func(t *testing.T) {})
    t.Run(\"second case\", func(t *testing.T) {})
}
";
        assert_eq!(
            parse(src),
            vec![
                ("TestDirect".into(), None),
                ("TestDirect".into(), Some("first case".into())),
                ("TestDirect".into(), Some("second case".into())),
            ]
        );
    }

    #[test]
    fn finds_table_driven_subtests() {
        let src = "\
package foo

func TestTable(t *testing.T) {
    cases := []struct {
        name string
        in   int
    }{
        {
            name: \"adds zero\",
            in:   0,
        },
        {
            name: \"adds one\",
            in:   1,
        },
    }
    for _, tc := range cases {
        t.Run(tc.name, func(t *testing.T) {})
    }
}
";
        assert_eq!(
            parse(src),
            vec![
                ("TestTable".into(), None),
                ("TestTable".into(), Some("adds zero".into())),
                ("TestTable".into(), Some("adds one".into())),
            ]
        );
    }

    #[test]
    fn dedups_within_function_but_not_across() {
        let src = "\
package foo

func TestOne(t *testing.T) {
    t.Run(\"shared\", func(t *testing.T) {})
    t.Run(\"shared\", func(t *testing.T) {})
}

func TestTwo(t *testing.T) {
    t.Run(\"shared\", func(t *testing.T) {})
}
";
        let got = parse(src);
        // Sort is done by find_go_tests_in_dir, not parse_go_test_file, so
        // the raw output is file-order. TestOne first, then TestTwo.
        assert_eq!(
            got,
            vec![
                ("TestOne".into(), None),
                ("TestOne".into(), Some("shared".into())),
                ("TestTwo".into(), None),
                ("TestTwo".into(), Some("shared".into())),
            ]
        );
    }

    #[test]
    fn rejects_escape_sequences_and_slashes() {
        let src = "\
package foo

func TestTricky(t *testing.T) {
    t.Run(\"has\\\"quote\", func(t *testing.T) {})
    t.Run(\"has/slash\", func(t *testing.T) {})
    t.Run(\"ok\", func(t *testing.T) {})
}
";
        // The backslash and slash cases are rejected; only `ok` survives.
        assert_eq!(
            parse(src),
            vec![
                ("TestTricky".into(), None),
                ("TestTricky".into(), Some("ok".into())),
            ]
        );
    }

    #[test]
    fn skips_non_test_funcs() {
        let src = "\
package foo

func helper() {}
func BenchmarkFoo(b *testing.B) {}
func FuzzFoo(f *testing.F) {}
func Test() {}

func TestReal(t *testing.T) {}
";
        assert_eq!(parse(src), vec![("TestReal".into(), None)]);
    }

    #[test]
    fn run_regex_for_parent_is_anchored() {
        let entry = GoTestEntry {
            name: "TestFoo".into(),
            subtest: None,
            file: "x.go".into(),
        };
        assert_eq!(go_test_run_regex(&entry), "^TestFoo$");
    }

    #[test]
    fn run_regex_for_subtest_uses_space_to_underscore_and_escapes() {
        let entry = GoTestEntry {
            name: "TestFoo".into(),
            subtest: Some("simple add (a.b)".into()),
            file: "x.go".into(),
        };
        // Space → underscore; `.` and parens escaped.
        assert_eq!(
            go_test_run_regex(&entry),
            r"^TestFoo$/^simple_add_\(a\.b\)$"
        );
    }
}
