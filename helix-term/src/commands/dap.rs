use super::{Context, Editor};
use crate::{
    compositor::{self, Compositor},
    dap_display::{load_byte_collection, DecodedBytes},
    job::{Callback, Jobs},
    ui::{self, overlay::overlaid, DebugVariables, Picker, Prompt, PromptEvent},
};
use dap::{StackFrame, Thread, ThreadStates};
use helix_core::syntax::config::{DebugAdapterConfig, DebugConfigCompletion, DebugTemplate};
use helix_core::{Selection, Transaction};
use helix_dap::{self as dap, requests::TerminateArguments};
use helix_lsp::block_on;
use helix_view::{
    editor::{Breakpoint, LastDebugLaunch},
    DocumentId, ViewId,
};

use serde_json::{to_value, Value};

use std::collections::HashSet;
use std::future::Future;
use std::io::{self, SeekFrom};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncSeekExt},
    sync::watch,
    time::{sleep, Duration},
};

use helix_view::handlers::dap::{breakpoints_changed, jump_to_stack_frame, select_thread_id};

const DEBUG_OUTPUT_TAIL_POLL_INTERVAL: Duration = Duration::from_millis(100);

const DAP_EVAL_INPUT_BUFFER_NAME: &str = "[dap-eval-input]";
const DAP_EVAL_RESULT_BUFFER_NAME: &str = "[dap-eval]";
const EVAL_VARIABLE_DEPTH_LIMIT: usize = 3;
const EVAL_VARIABLE_NODE_LIMIT: usize = 512;
const GO_EVAL_START_MARKER: &str = "/*__hx_dap_expr_start__*/";
const GO_EVAL_END_MARKER: &str = "/*__hx_dap_expr_end__*/";
const GO_EVAL_INDENT: &str = "        ";

struct EvalInputSeed {
    content: String,
    cursor: usize,
    language: Option<String>,
    source_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct ResolvedDebugLaunch {
    template_name: String,
    request_type: String,
    args: Value,
}

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

fn dap_start_callback(
    jobs: &mut Jobs,
    id: dap::registry::DebugAdapterId,
    request_type: String,
    call: impl Future<Output = helix_dap::Result<serde_json::Value>> + 'static + Send,
) {
    let callback = Box::pin(async move {
        let result = call.await;
        let call: Callback = Callback::Editor(Box::new(move |editor: &mut Editor| match result {
            Ok(_) => {
                editor.set_status(format!("Debug {} request accepted", request_type));
            }
            Err(err) => {
                editor.stop_debug_output_tails(id);
                editor.debug_adapters.remove_client(id);
                if editor
                    .debug_adapters
                    .get_active_client()
                    .map(|client| client.id())
                    == Some(id)
                {
                    editor.debug_adapters.unset_active_client();
                }
                editor.set_error(format!("Failed to start debug session: {}", err));
            }
        }));
        Ok(call)
    });

    jobs.callback(callback);
}

fn redirected_output_paths(args: &Value) -> Vec<(PathBuf, &'static str, bool)> {
    let mut paths = Vec::new();
    let Some(args) = args.as_object() else {
        return paths;
    };

    for (field, prefix, is_stderr) in [
        ("stdoutTo", "[stdout] [debuggee]", false),
        ("stderrTo", "[stderr] [debuggee]", true),
    ] {
        if let Some(path) = args.get(field).and_then(Value::as_str) {
            paths.push((PathBuf::from(path), prefix, is_stderr));
        }
    }

    paths
}

fn register_redirected_output_tails(
    editor: &mut Editor,
    id: dap::registry::DebugAdapterId,
    args: &Value,
) {
    for (path, prefix, is_stderr) in redirected_output_paths(args) {
        let (stop_tx, stop_rx) = watch::channel(false);
        editor.register_debug_output_tail(id, stop_tx);
        tokio::spawn(tail_redirected_output(path, prefix, is_stderr, stop_rx));
    }
}

async fn tail_redirected_output(
    path: PathBuf,
    prefix: &'static str,
    is_stderr: bool,
    mut stop_rx: watch::Receiver<bool>,
) {
    let mut offset = redirected_output_len(&path).await.unwrap_or(0);

    loop {
        let should_stop = tokio::select! {
            changed = stop_rx.changed() => changed.is_err() || *stop_rx.borrow(),
            _ = sleep(DEBUG_OUTPUT_TAIL_POLL_INTERVAL) => false,
        };

        match read_redirected_output(&path, &mut offset).await {
            Ok(Some(output)) if !output.is_empty() => {
                crate::job::dispatch(move |editor, _compositor| {
                    editor.push_debug_output(prefix, &output, is_stderr);
                })
                .await;
            }
            Ok(_) => {}
            Err(err) => {
                log::warn!("failed to tail debug output from {}: {err}", path.display());
                break;
            }
        }

        if should_stop {
            break;
        }
    }
}

async fn redirected_output_len(path: &Path) -> Option<u64> {
    fs::metadata(path).await.ok().map(|metadata| metadata.len())
}

async fn read_redirected_output(path: &Path, offset: &mut u64) -> io::Result<Option<String>> {
    let len = match fs::metadata(path).await {
        Ok(metadata) => metadata.len(),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            *offset = 0;
            return Ok(None);
        }
        Err(err) => return Err(err),
    };

    if len < *offset {
        *offset = 0;
    }

    if len == *offset {
        return Ok(None);
    }

    let mut file = fs::File::open(path).await?;
    file.seek(SeekFrom::Start(*offset)).await?;

    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer).await?;
    *offset += buffer.len() as u64;

    Ok(Some(String::from_utf8_lossy(&buffer).into_owned()))
}

pub fn dap_start_impl(
    cx: &mut compositor::Context,
    name: Option<&str>,
    socket: Option<SocketAddr>,
    params: Option<Vec<std::borrow::Cow<str>>>,
) -> Result<(), anyhow::Error> {
    let config = doc!(cx.editor)
        .language_config()
        .and_then(|config| config.debugger.as_ref())
        .ok_or_else(|| anyhow!("No debug adapter available for language"))?
        .clone();
    let params = params.map(|params| {
        params
            .into_iter()
            .map(|param| param.into_owned())
            .collect::<Vec<_>>()
    });
    let resolved = resolve_debug_launch(&config, name, params.as_deref())?;

    start_debug_launch(
        cx.editor,
        cx.jobs,
        LastDebugLaunch {
            config,
            template_name: resolved.template_name,
            request_type: resolved.request_type,
            socket,
            args: resolved.args,
        },
    )
}

fn resolve_debug_launch(
    config: &DebugAdapterConfig,
    name: Option<&str>,
    params: Option<&[String]>,
) -> Result<ResolvedDebugLaunch, anyhow::Error> {
    let template = match name {
        Some(name) => config.templates.iter().find(|t| t.name == name),
        None => config.templates.first(),
    }
    .ok_or_else(|| anyhow!("No debug config with given name"))?;

    let mut args: serde_json::Map<String, Value> = if let Some(params) = params {
        let preprocessed_params = prepare_dap_params(template, params);
        template
            .args
            .iter()
            .map(|(k, v)| (k.clone(), map_value(v, &preprocessed_params)))
            .collect()
    } else {
        template
            .args
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    };

    if !args.contains_key("cwd") {
        args.insert(
            "cwd".to_owned(),
            to_value(helix_stdx::env::current_working_dir())?,
        );
    }

    Ok(ResolvedDebugLaunch {
        template_name: template.name.clone(),
        request_type: template.request.clone(),
        args: Value::Object(args),
    })
}

fn start_debug_launch(
    editor: &mut Editor,
    jobs: &mut Jobs,
    launch: LastDebugLaunch,
) -> Result<(), anyhow::Error> {
    match launch.request_type.as_str() {
        "launch" | "attach" => {}
        request => bail!("Unsupported request '{}'", request),
    }

    editor.set_status("Starting debug adapter...");
    editor.stop_all_debug_output_tails();
    editor.clear_debug_output();
    editor.last_debug_launch = Some(launch.clone());

    let id = editor
        .debug_adapters
        .start_client(launch.socket, &launch.config)
        .map_err(|e| anyhow!("Failed to start debug client: {}", e))?;

    register_redirected_output_tails(editor, id, &launch.args);

    let debugger = match editor.debug_adapters.get_client_mut(id) {
        Some(child) => child,
        None => {
            bail!("Failed to get child debugger.");
        }
    };

    let args = launch.args.clone();
    match launch.request_type.as_str() {
        "launch" => {
            let call = debugger.launch(args);
            dap_start_callback(jobs, id, "launch".to_owned(), call);
        }
        "attach" => {
            let call = debugger.attach(args);
            dap_start_callback(jobs, id, "attach".to_owned(), call);
        }
        _ => unreachable!("validated request type"),
    };

    Ok(())
}

fn prepare_dap_params(template: &DebugTemplate, params: &[String]) -> Vec<String> {
    params
        .iter()
        .enumerate()
        .map(|(i, x)| {
            let mut param = x.to_string();
            if let Some(DebugConfigCompletion::Advanced(cfg)) = template.completion.get(i) {
                if matches!(cfg.completion.as_deref(), Some("filename" | "directory")) {
                    param = std::fs::canonicalize(x)
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
pub(super) struct GoTestEntry {
    /// Bare function name, e.g. `TestDownloadArtifacts`.
    pub(super) name: String,
    /// The specific subtest case, in source form (spaces preserved for
    /// display). `None` means "the whole test function".
    pub(super) subtest: Option<String>,
    /// The `_test.go` file the function is declared in (basename only,
    /// for display in the picker).
    pub(super) file: String,
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
pub(super) fn find_go_tests_in_dir(dir: &Path) -> Vec<GoTestEntry> {
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
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
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
pub(super) fn go_test_run_regex(entry: &GoTestEntry) -> String {
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
///
/// If the template defines additional completion entries after the initial
/// `go-test-function` slot, selection instead chains into
/// `debug_parameter_prompt` with `[pkg_dir, test_regex]` as the initial
/// params, so the remaining fields (e.g. godog tags) can be typed before
/// launch.
fn build_go_test_picker(
    config_name: String,
    pkg_dir: PathBuf,
    tests: Vec<GoTestEntry>,
    completions: Vec<DebugConfigCompletion>,
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
        let pkg = pkg_dir_str.clone();
        let test_regex = go_test_run_regex(entry);

        if completions.len() > 1 {
            let completions = completions.clone();
            let config_name = config_name.clone();
            let initial_params = vec![pkg, test_regex];
            let callback = Box::pin(async move {
                let call: Callback =
                    Callback::EditorCompositor(Box::new(move |_editor, compositor| {
                        let prompt =
                            debug_parameter_prompt(completions, config_name, initial_params);
                        compositor.push(Box::new(prompt));
                    }));
                Ok(call)
            });
            cx.jobs.callback(callback);
        } else {
            let params: Vec<std::borrow::Cow<str>> = vec![pkg.into(), test_regex.into()];
            if let Err(err) = dap_start_impl(cx, Some(&config_name), None, Some(params)) {
                cx.editor.set_error(err.to_string());
            }
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
                let completions = template.completion.clone();
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
                            let picker = build_go_test_picker(name, pkg_dir, tests, completions);
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

pub(crate) fn rerun_last_debug_launch(editor: &mut Editor, jobs: &mut Jobs) {
    if editor.debug_adapters.get_active_client().is_some() {
        editor.set_error("Debugger is already running");
        return;
    }

    let Some(launch) = editor.last_debug_launch.clone() else {
        editor.set_error("No previous debug launch to rerun");
        return;
    };

    if let Err(err) = start_debug_launch(editor, jobs, launch) {
        editor.set_error(err.to_string());
    }
}

pub fn dap_rerun_last(cx: &mut Context) {
    rerun_last_debug_launch(cx.editor, cx.jobs);
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
    if is_current_eval_input_buffer(cx.editor) {
        let expression = current_selection_or_buffer(cx.editor);
        if expression.trim().is_empty() {
            cx.editor.set_status("No expression to evaluate.");
            return;
        }

        evaluate_expression(cx.editor, expression, true);
        return;
    }

    let seed = current_eval_input_seed(cx.editor);
    show_eval_input_buffer(
        cx.editor,
        &seed.content,
        seed.cursor,
        seed.language.as_deref(),
        seed.source_path.as_deref(),
    );
    cx.editor.set_status(
        "Opened debug expression buffer. Use Space G e here to evaluate selection or buffer.",
    );
    super::enter_insert_mode(cx);
}

pub fn dap_eval_selection(cx: &mut Context) {
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

    evaluate_expression(cx.editor, expression, false);
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
fn show_eval_result_in_buffer(editor: &mut Editor, content: String, preserve_focus: bool) {
    use helix_view::editor::Action;

    let origin_view_id = preserve_focus.then(|| view!(editor).id);

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

            let target_view_id = match visible_view_id {
                Some(view_id) => {
                    if !preserve_focus {
                        editor.focus(view_id);
                    }
                    view_id
                }
                None => {
                    editor.switch(id, Action::VerticalSplit);
                    view!(editor).id
                }
            };

            replace_eval_result_buffer(editor, id, target_view_id, content);
        }
        None => {
            // First eval this session (or the previous buffer was closed).
            // Open a fresh scratch buffer in a vertical split and remember it.
            let doc_id = editor.new_file(Action::VerticalSplit);
            editor.debug_eval_doc_id = Some(doc_id);
            {
                let doc = doc_mut!(editor, &doc_id);
                doc.set_virtual_name(Some(DAP_EVAL_RESULT_BUFFER_NAME.to_string()));
            }
            replace_eval_result_buffer(editor, doc_id, view!(editor).id, content);
        }
    }

    if let Some(view_id) = origin_view_id {
        editor.focus(view_id);
    }
}

fn replace_eval_result_buffer(
    editor: &mut Editor,
    doc_id: DocumentId,
    view_id: ViewId,
    content: String,
) {
    let view = editor.tree.get_mut(view_id);
    let doc = doc_mut!(editor, &doc_id);
    doc.set_soft_wrap_override(Some(true));
    doc.ensure_view_init(view.id);

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

fn show_eval_input_buffer(
    editor: &mut Editor,
    content: &str,
    cursor: usize,
    language: Option<&str>,
    source_path: Option<&Path>,
) {
    use helix_view::editor::Action;

    let doc_id = match editor.debug_eval_input_doc_id {
        Some(id) if editor.documents.contains_key(&id) => {
            let visible_view_id = editor
                .tree
                .traverse()
                .find(|(_, view)| view.doc == id)
                .map(|(view_id, _)| view_id);

            match visible_view_id {
                Some(view_id) => editor.focus(view_id),
                None => editor.switch(id, Action::VerticalSplit),
            }

            id
        }
        _ => {
            let doc_id = editor.new_file(Action::VerticalSplit);
            editor.debug_eval_input_doc_id = Some(doc_id);
            doc_id
        }
    };

    let should_set_path = {
        let doc = doc!(editor, &doc_id);
        doc.path().is_none()
    };

    if should_set_path {
        if let Some(path) = source_path
            .map(|path| synthetic_eval_input_path(path, doc_id))
            .filter(|path| {
                !editor
                    .documents
                    .values()
                    .any(|doc| doc.path() == Some(path))
            })
        {
            editor.set_doc_path(doc_id, &path);
        }
    }

    let should_seed = {
        let doc = doc!(editor, &doc_id);
        !content.is_empty() && doc.text().len_chars() == 0 && !doc.is_modified()
    };

    let loader = editor.syn_loader.load();
    let view = view_mut!(editor);
    let doc = doc_mut!(editor, &doc_id);
    doc.set_virtual_name(Some(DAP_EVAL_INPUT_BUFFER_NAME.to_string()));
    doc.ensure_view_init(view.id);

    if let Some(language) = language {
        let _ = doc.set_language_by_language_id(language, &loader);
    }

    if should_seed {
        let transaction = Transaction::change(
            doc.text(),
            std::iter::once((0, doc.text().len_chars(), Some(content.into()))),
        )
        .with_selection(Selection::point(cursor));
        doc.apply(&transaction, view.id);
        doc.append_changes_to_history(view);
        doc.reset_modified();
    }
}

fn synthetic_eval_input_path(source_path: &Path, doc_id: DocumentId) -> PathBuf {
    let parent = source_path.parent().unwrap_or_else(|| Path::new("."));
    let extension = source_path
        .extension()
        .filter(|extension| !extension.is_empty())
        .map(|extension| format!(".{}", extension.to_string_lossy()))
        .unwrap_or_default();

    parent.join(format!("hx-dap-eval-input-{doc_id}{extension}"))
}

fn is_current_eval_input_buffer(editor: &mut Editor) -> bool {
    let Some(doc_id) = editor.debug_eval_input_doc_id else {
        return false;
    };
    if !editor.documents.contains_key(&doc_id) {
        return false;
    }

    let (_, doc) = current!(editor);
    doc.id() == doc_id
}

fn current_eval_input_seed(editor: &mut Editor) -> EvalInputSeed {
    let (view, doc) = current!(editor);
    let text = doc.text().slice(..);
    let primary = doc.selection(view.id).primary();
    let prefill = if primary.len() > 1 {
        primary.fragment(text).to_string()
    } else {
        String::new()
    };
    let language = doc.language_name().map(str::to_owned);
    let source_path = doc.path().cloned();

    match language.as_deref() {
        Some("go") => build_go_eval_input_seed(&text.to_string(), prefill, source_path),
        _ => EvalInputSeed {
            cursor: prefill.chars().count(),
            content: prefill,
            language,
            source_path,
        },
    }
}

fn current_selection_or_buffer(editor: &mut Editor) -> String {
    let (view, doc) = current!(editor);
    let text = doc.text().slice(..);
    let primary = doc.selection(view.id).primary();

    if primary.len() > 1 {
        primary.fragment(text).to_string()
    } else if doc.language_name() == Some("go") {
        extract_go_eval_expression(&text.to_string()).unwrap_or_else(|| text.to_string())
    } else {
        text.to_string()
    }
}

fn build_go_eval_input_seed(
    source_text: &str,
    expression: String,
    source_path: Option<PathBuf>,
) -> EvalInputSeed {
    let package = detect_go_package_name(source_text).unwrap_or("main");
    let import_block = extract_go_import_block(source_text);

    let mut content = format!("package {package}\n");
    if let Some(import_block) = import_block {
        content.push('\n');
        content.push_str(&import_block);
        content.push('\n');
    }
    content.push_str("\nfunc __hx_dap_eval__() any {\n    return (\n");
    content.push_str(GO_EVAL_INDENT);
    content.push_str(GO_EVAL_START_MARKER);
    content.push('\n');

    if expression.is_empty() {
        content.push_str(GO_EVAL_INDENT);
        content.push('\n');
    } else {
        for line in expression.lines() {
            content.push_str(GO_EVAL_INDENT);
            content.push_str(line);
            content.push('\n');
        }
    }

    content.push_str(GO_EVAL_INDENT);
    content.push_str(GO_EVAL_END_MARKER);
    content.push_str("\n    )\n}\n");

    let cursor = go_eval_expression_region(&content)
        .map(|(_, end)| content[..end].chars().count())
        .unwrap_or_else(|| content.chars().count());

    EvalInputSeed {
        content,
        cursor,
        language: Some("go".to_string()),
        source_path,
    }
}

fn detect_go_package_name(source: &str) -> Option<&str> {
    source.lines().find_map(|line| {
        let trimmed = line.trim();
        trimmed
            .strip_prefix("package ")
            .and_then(|rest| rest.split_whitespace().next())
    })
}

fn extract_go_import_block(source: &str) -> Option<String> {
    let lines: Vec<_> = source.lines().collect();
    let package_index = lines
        .iter()
        .position(|line| line.trim().starts_with("package "))?;

    let mut index = package_index + 1;
    while index < lines.len() {
        let trimmed = lines[index].trim();
        if trimmed.is_empty() || trimmed.starts_with("//") {
            index += 1;
            continue;
        }

        if trimmed.starts_with("import ") {
            if trimmed == "import (" || trimmed.starts_with("import (") {
                let start = index;
                index += 1;
                while index < lines.len() && lines[index].trim() != ")" {
                    index += 1;
                }
                if index < lines.len() {
                    index += 1;
                }
                return Some(lines[start..index].join("\n"));
            }

            let start = index;
            index += 1;
            while index < lines.len() && lines[index].trim().starts_with("import ") {
                index += 1;
            }
            return Some(lines[start..index].join("\n"));
        }

        return None;
    }

    None
}

fn go_eval_expression_region(text: &str) -> Option<(usize, usize)> {
    let start_marker = text.find(GO_EVAL_START_MARKER)? + GO_EVAL_START_MARKER.len();
    let end_marker = text[start_marker..].find(GO_EVAL_END_MARKER)? + start_marker;

    let mut start = start_marker;
    let mut end = end_marker;

    if text[start..end].starts_with('\n') {
        start += 1;
    }
    if start < end && text[start..end].ends_with('\n') {
        end -= 1;
    }

    Some((start, end))
}

fn extract_go_eval_expression(text: &str) -> Option<String> {
    let (start, end) = go_eval_expression_region(text)?;
    let inner = &text[start..end];
    let dedented = inner
        .lines()
        .map(|line| line.strip_prefix(GO_EVAL_INDENT).unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n");

    Some(dedented.trim_end().to_string())
}

fn current_stack_frame_id(editor: &Editor) -> Result<usize, &'static str> {
    let Some(debugger) = editor.debug_adapters.get_active_client() else {
        return Err("No active debug session.");
    };

    if debugger.thread_id.is_none() {
        return Err("Cannot evaluate while target is running.");
    }

    let (frame, thread_id) = match (debugger.active_frame, debugger.thread_id) {
        (Some(frame), Some(thread_id)) => (frame, thread_id),
        _ => return Err("Cannot find current stack frame to evaluate."),
    };

    Ok(debugger.stack_frames[&thread_id][frame].id)
}

fn evaluate_expression(editor: &mut Editor, expression: String, preserve_focus: bool) {
    let frame_id = match current_stack_frame_id(editor) {
        Ok(frame_id) => frame_id,
        Err(message) => {
            editor.set_status(message);
            return;
        }
    };

    let response = {
        let debugger = debugger!(editor);
        block_on(debugger.eval(expression.clone(), Some(frame_id)))
    };

    match response {
        Ok(resp) => {
            let text = {
                let debugger = debugger!(editor);
                format_eval_response(debugger, &expression, &resp)
            };
            show_eval_result_in_buffer(editor, text, preserve_focus);
        }
        Err(e) => editor.set_error(format!("Eval '{}': {}", expression, e)),
    }
}

fn format_eval_response(
    debugger: &dap::Client,
    expression: &str,
    resp: &dap::requests::EvaluateResponse,
) -> String {
    let header = resp
        .ty
        .as_ref()
        .map(|t| format!("{expression}: {t}"))
        .unwrap_or_else(|| expression.to_owned());

    if let Some(children) =
        format_eval_children(debugger, resp.variables_reference, resp.indexed_variables)
    {
        return format!("{header}\n\n{children}");
    }

    let summary = if resp.result.is_empty() {
        "(empty)".to_string()
    } else {
        format_eval_value(&resp.result)
    };

    format!("{header}\n\n{summary}")
}

fn format_eval_children(
    debugger: &dap::Client,
    variables_reference: usize,
    indexed_variables: Option<usize>,
) -> Option<String> {
    if variables_reference == 0 {
        return None;
    }

    if let Some(decoded) = load_byte_collection(debugger, variables_reference, indexed_variables) {
        return Some(decoded.pretty);
    }

    let response = block_on(debugger.variables(variables_reference)).ok()?;

    let mut visited = HashSet::new();
    let mut lines = Vec::new();
    let mut remaining = EVAL_VARIABLE_NODE_LIMIT;

    match render_variable_children_from_response(
        debugger,
        response,
        0,
        &mut remaining,
        &mut visited,
        &mut lines,
    ) {
        Ok(()) => {}
        Err(err) => lines.push(format!("<failed to expand children: {err}>")),
    }

    if lines.is_empty() {
        None
    } else {
        Some(format!("children:\n{}", lines.join("\n")))
    }
}

fn render_variable_children_from_response(
    debugger: &dap::Client,
    variables: Vec<dap::Variable>,
    depth: usize,
    remaining: &mut usize,
    visited: &mut HashSet<usize>,
    lines: &mut Vec<String>,
) -> helix_dap::Result<()> {
    for variable in variables {
        if *remaining == 0 {
            lines.push(format!("{}...", "  ".repeat(depth)));
            break;
        }

        *remaining -= 1;

        if variable.variables_reference != 0
            && depth < EVAL_VARIABLE_DEPTH_LIMIT
            && visited.insert(variable.variables_reference)
        {
            if let Some(decoded) = load_byte_collection(
                debugger,
                variable.variables_reference,
                variable.indexed_variables,
            ) {
                push_decoded_variable_lines(lines, &variable, depth, decoded);
                continue;
            }

            let children = block_on(debugger.variables(variable.variables_reference))?;

            push_variable_lines(lines, &variable, depth);
            render_variable_children_from_response(
                debugger,
                children,
                depth + 1,
                remaining,
                visited,
                lines,
            )?;
        } else {
            push_variable_lines(lines, &variable, depth);
        }
    }

    Ok(())
}

fn push_variable_lines(lines: &mut Vec<String>, variable: &dap::Variable, depth: usize) {
    let indent = "  ".repeat(depth);
    let ty = variable
        .ty
        .as_ref()
        .map(|ty| format!(": {ty}"))
        .unwrap_or_default();
    let formatted = format_eval_value(&variable.value);

    if formatted.contains('\n') {
        lines.push(format!("{indent}{}{ty} =", variable.name));
        push_indented_block(lines, &formatted, depth + 1);
    } else {
        lines.push(format!("{indent}{}{ty} = {formatted}", variable.name));
    }
}

fn push_decoded_variable_lines(
    lines: &mut Vec<String>,
    variable: &dap::Variable,
    depth: usize,
    decoded: DecodedBytes,
) {
    let indent = "  ".repeat(depth);
    let ty = variable
        .ty
        .as_ref()
        .map(|ty| format!(": {ty}"))
        .unwrap_or_default();

    if decoded.pretty.contains('\n') {
        lines.push(format!("{indent}{}{ty} =", variable.name));
        push_indented_block(lines, &decoded.pretty, depth + 1);
    } else {
        lines.push(format!(
            "{indent}{}{ty} = {}",
            variable.name, decoded.pretty
        ));
    }
}

fn push_indented_block(lines: &mut Vec<String>, text: &str, depth: usize) {
    let indent = "  ".repeat(depth);
    lines.extend(text.lines().map(|line| format!("{indent}{line}")));
}

/// Format a DAP eval result for display: break composite values onto separate lines.
fn format_eval_value(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut depth = 0usize;
    let mut chars = value.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;

    while let Some(ch) = chars.next() {
        if in_string {
            result.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => {
                in_string = true;
                result.push(ch);
            }
            '{' | '[' => {
                depth += 1;
                result.push(ch);
                result.push('\n');
                for _ in 0..depth {
                    result.push_str("  ");
                }
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                result.push('\n');
                for _ in 0..depth {
                    result.push_str("  ");
                }
                result.push(ch);
            }
            ',' => {
                result.push(ch);
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
    .with_preview(|_editor, item| Some((item.path.as_path().into(), Some((item.line, item.line)))));
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
                        cx.editor.set_status(format!("Added watch: {}", expr));
                    } else {
                        cx.editor.set_status(format!("Already watching: {}", expr));
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
        cx.editor.set_status(format!("Removed watch: {}", expr));
    });
    cx.push_layer(Box::new(picker));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

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

    #[test]
    fn finds_redirected_debuggee_output_paths() {
        let args = serde_json::json!({
            "stdoutTo": "/tmp/debuggee-stdout.log",
            "stderrTo": "/tmp/debuggee-stderr.log",
        });

        assert_eq!(
            redirected_output_paths(&args),
            vec![
                (
                    PathBuf::from("/tmp/debuggee-stdout.log"),
                    "[stdout] [debuggee]",
                    false,
                ),
                (
                    PathBuf::from("/tmp/debuggee-stderr.log"),
                    "[stderr] [debuggee]",
                    true,
                ),
            ]
        );
    }

    #[test]
    fn resolves_debug_launch_with_template_args() {
        let config = DebugAdapterConfig {
            name: "go".into(),
            transport: "stdio".into(),
            command: "dlv".into(),
            args: Vec::new(),
            port_arg: None,
            quirks: Default::default(),
            templates: vec![DebugTemplate {
                name: "go-test".into(),
                request: "launch".into(),
                completion: vec![DebugConfigCompletion::Named("package".into())],
                args: HashMap::from([
                    ("mode".into(), Value::String("test".into())),
                    ("program".into(), Value::String("{0}".into())),
                    ("cwd".into(), Value::String("/tmp/work".into())),
                ]),
            }],
        };

        let resolved = resolve_debug_launch(&config, None, Some(&["./pkg".to_owned()])).unwrap();

        assert_eq!(resolved.template_name, "go-test");
        assert_eq!(resolved.request_type, "launch");
        assert_eq!(
            resolved.args,
            serde_json::json!({
                "mode": "test",
                "program": "./pkg",
                "cwd": "/tmp/work",
            })
        );
    }

    #[test]
    fn builds_eval_input_path_next_to_source_file() {
        let path =
            synthetic_eval_input_path(Path::new("/tmp/example/main.go"), DocumentId::default());

        assert_eq!(path, PathBuf::from("/tmp/example/hx-dap-eval-input-1.go"));
    }

    #[test]
    fn formats_bracketed_eval_values_across_lines() {
        assert_eq!(
            format_eval_value(r#"["a", "b", "c"]"#),
            "[\n  \"a\",\n  \"b\",\n  \"c\"\n]"
        );
    }

    #[test]
    fn extracts_go_eval_expression_from_wrapper() {
        let seed = build_go_eval_input_seed(
            "package foo\n\nimport \"os\"\n",
            "os.Getenv(\"HOME\")".to_string(),
            None,
        );

        assert_eq!(
            extract_go_eval_expression(&seed.content),
            Some("os.Getenv(\"HOME\")".to_string())
        );
    }

    #[test]
    fn preserves_go_import_block_in_eval_seed() {
        let seed = build_go_eval_input_seed(
            "package foo\n\nimport (\n    \"fmt\"\n    \"os\"\n)\n\nfunc main() {}\n",
            String::new(),
            None,
        );

        assert!(seed
            .content
            .contains("import (\n    \"fmt\"\n    \"os\"\n)"));
        assert!(seed.content.contains("func __hx_dap_eval__() any {"));
    }
}
