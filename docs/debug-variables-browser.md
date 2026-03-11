# Debug Variables Browser

This document describes the local MVP for the "locals" part of the debugging overhaul proposed in <https://github.com/helix-editor/helix/issues/5950>.

## Scope

The change replaces the old `dap_variables` popup, which rendered a flat list of scopes and variables, with a dedicated browser for debug variables.

Current behavior:

- `dap_variables` (`<space>Gv`) opens an overlaid variables browser.
- Scopes are sorted to prefer `locals`, then `arguments`, then `returnValue`, then `registers`, then everything else.
- The first `locals` scope is expanded automatically on open.
- Child variables are loaded lazily from the active debug adapter when expanded.
- Nested values can be expanded repeatedly as long as the adapter provides a non-zero `variablesReference`.
- Expansion errors are surfaced both inline in the browser and through the editor error/status channel.

Current navigation:

- `j` / `Down`: move down
- `k` / `Up`: move up
- `h` / `Left`: collapse current node, or move to its parent
- `l` / `Right` / `Enter`: expand current node
- `PageUp` / `Ctrl-u`: move up by a page
- `PageDown` / `Ctrl-d`: move down by a page
- `Home`: jump to the first row
- `End`: jump to the last row
- `Esc` / `Ctrl-c`: close the browser

## Files

The implementation is split across these files:

- `helix-term/src/ui/debug.rs`: tree-like browser component, lazy loading, rendering, and navigation
- `helix-term/src/ui/mod.rs`: exports the new UI component
- `helix-term/src/commands/dap.rs`: switches `dap_variables` to the new browser
- `helix-term/src/commands.rs`: updates the command description to "Open debug variables browser"

## Non-goals

This is intentionally smaller than the full overhaul in the issue.

Not implemented here:

- a dockable debug side panel
- watches
- editable variable values
- inline quick watch UI
- persistent expansion state across browser reopen

## Automated verification

From the repository root:

```bash
nix develop -c cargo fmt --all
nix develop -c cargo test -p helix-term --lib
```

The `helix-term` test target covers the added unit tests for scope ordering and visible tree expansion bookkeeping.

## Manual test plan

Prerequisite: use a language/debugger combination that already has a working Helix DAP configuration. Rust is a practical option if you already have a configured adapter such as `lldb-dap` or `codelldb`, but adapter setup is outside the scope of this document.

1. Open a file with a valid debug adapter configuration.
2. Set a breakpoint with `<space>Gb`.
3. Launch the debugger with `<space>Gl`.
4. Stop at the breakpoint so the current thread/frame has scopes available.
5. Open the variables browser with `<space>Gv`.
6. Verify that the `Locals` scope appears first and is already expanded.
7. Move through the tree with `j` / `k` or the arrow keys.
8. Expand nested values with `l`, `Right`, or `Enter`.
9. Collapse them with `h` or `Left`.
10. Confirm that large or nested values are fetched only when you expand them.
11. Close the browser with `Esc`.

Useful negative checks:

- Trigger `<space>Gv` while the program is still running and confirm Helix reports that variables cannot be accessed while the target is running.
- Expand a node from an adapter/session that returns an error and confirm the error is visible both inline and in the editor status/error reporting.

## Expected UX

Compared with the previous implementation, the new browser should make it practical to inspect structured values without flooding the popup with every child value up front. The browser is still an overlay, but it now behaves like a focused locals/variables view instead of a static text dump.
