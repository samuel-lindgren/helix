//! Conservative cursor selection. Never infer a table row's runtime name from
//! an unrelated `name` field, or select the first instance of a repeated name.
use helix_core::tree_sitter::Node;

use super::GoTestEntry;

pub(super) struct Target {
    pub entry: GoTestEntry,
    pub note: Option<String>,
}

fn field<'a>(node: &Node<'a>, name: &str) -> Option<Node<'a>> {
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

fn text<'a>(node: &Node<'_>, source: &'a str) -> &'a str {
    &source[node.start_byte() as usize..node.end_byte() as usize]
}

fn children<'a>(node: &Node<'a>) -> impl Iterator<Item = Node<'a>> {
    node.children().filter(|n| n.is_named() && !n.is_extra())
}

fn descendants<'a>(node: &Node<'a>) -> impl Iterator<Item = Node<'a>> {
    let mut pending = vec![node.clone()];
    std::iter::from_fn(move || {
        let node = pending.pop()?;
        pending.extend(node.children());
        Some(node)
    })
}

fn contains(node: &Node<'_>, byte: usize) -> bool {
    (node.start_byte() as usize..node.end_byte() as usize).contains(&byte)
}

// Restrict names to literals that round-trip through the existing runner's
// normalization. Escapes, slash paths and Go's generated #NN suffixes need
// runtime information; a parent run is more honest than a guessed case.
fn literal<'a>(node: &Node<'_>, source: &'a str) -> Option<&'a str> {
    if !matches!(
        node.kind(),
        "interpreted_string_literal" | "raw_string_literal"
    ) {
        return None;
    }
    let raw = text(node, source);
    let quote = if node.kind() == "raw_string_literal" {
        '`'
    } else {
        '"'
    };
    let value = raw.strip_prefix(quote)?.strip_suffix(quote)?;
    (!value.is_empty()
        && !value.chars().any(|c| {
            matches!(c, '\\' | '/' | '#') || c.is_control() || (c.is_whitespace() && c != ' ')
        }))
    .then_some(value)
}

fn testing_import<'a>(root: &Node<'_>, source: &'a str) -> Option<&'a str> {
    descendants(root)
        .filter(|node| node.kind() == "import_spec")
        .find_map(|node| {
            (literal(&field(&node, "path")?, source)? == "testing").then(|| {
                field(&node, "name")
                    .map(|name| text(&name, source))
                    .unwrap_or("testing")
            })
        })
}

fn receiver<'a>(function: &Node<'_>, source: &'a str, import: &str) -> Option<&'a str> {
    if field(function, "result").is_some() || field(function, "type_parameters").is_some() {
        return None;
    }
    let parameters = field(function, "parameters")?;
    let mut parameters = children(&parameters);
    let param = parameters.next()?;
    if parameters.next().is_some() || param.kind() != "parameter_declaration" {
        return None;
    }
    // Reject grouped parameters, e.g. (a, b *testing.T).
    if children(&param)
        .filter(|n| n.kind() == "identifier")
        .count()
        > 1
    {
        return None;
    }
    let ty = field(&param, "type")?;
    if ty.kind() != "pointer_type" {
        return None;
    }
    let ty = children(&ty).next()?;
    let valid = if import == "." {
        ty.kind() == "type_identifier" && text(&ty, source) == "T"
    } else {
        ty.kind() == "qualified_type"
            && text(&field(&ty, "package")?, source) == import
            && text(&field(&ty, "name")?, source) == "T"
    };
    valid.then(|| {
        field(&param, "name")
            .map(|n| text(&n, source))
            .unwrap_or("")
    })
}

fn run_receiver<'a>(node: &Node<'_>, source: &'a str) -> Option<&'a str> {
    if node.kind() != "call_expression" {
        return None;
    }
    let function = field(node, "function")?;
    if function.kind() != "selector_expression"
        || text(&field(&function, "field")?, source) != "Run"
    {
        return None;
    }
    Some(text(&field(&function, "operand")?, source))
}

pub(super) fn at_cursor(
    root: Node<'_>,
    source: &str,
    byte: usize,
    file: &str,
) -> anyhow::Result<Target> {
    // Leading indentation belongs to the declaration/call on this line. Do
    // not cross a newline and accidentally choose the next test or sibling.
    let byte = byte
        + source[byte..]
            .bytes()
            .take_while(|b| matches!(b, b' ' | b'\t'))
            .count();
    let function = children(&root)
        .find(|n| n.kind() == "function_declaration" && contains(n, byte))
        .ok_or_else(|| anyhow::anyhow!("Cursor is not inside a Go test function"))?;
    anyhow::ensure!(
        !descendants(&function).any(|n| n.kind() == "ERROR" || n.is_missing()),
        "Fix syntax errors in the test before running it"
    );
    let name = field(&function, "name")
        .map(|n| text(&n, source))
        .unwrap_or("");
    let receiver =
        testing_import(&root, source).and_then(|import| receiver(&function, source, import));
    anyhow::ensure!(
        name.strip_prefix("Test")
            .is_some_and(|suffix| { !suffix.chars().next().is_some_and(char::is_lowercase) })
            && receiver.is_some(),
        "Cursor is not inside a Go test function (func TestX(*testing.T))"
    );
    let receiver = receiver.unwrap();
    let mut target = Target {
        entry: GoTestEntry {
            name: name.into(),
            subtest: None,
            file: file.into(),
        },
        note: None,
    };
    let body =
        field(&function, "body").ok_or_else(|| anyhow::anyhow!("The test function has no body"))?;

    // Stay in the test's scope. Callback bodies belong to a subtest or an
    // opaque closure, not to its statically enumerable sibling calls.
    let mut pending = vec![body.clone()];
    let mut calls = Vec::new();
    let mut opaque = false;
    while let Some(node) = pending.pop() {
        if node.kind() == "func_literal" {
            // A closure outside a direct t.Run can generate unknown siblings.
            let direct_callback = node
                .parent()
                .and_then(|n| n.parent())
                .is_some_and(|call| run_receiver(&call, source) == Some(receiver));
            opaque |= !direct_callback;
            continue;
        }
        // Rebinding the test receiver makes textual t.Run resolution unsafe.
        if matches!(
            node.kind(),
            "short_var_declaration"
                | "assignment_statement"
                | "range_clause"
                | "receive_statement"
                | "type_switch_statement"
        ) {
            let binding = field(&node, "left").or_else(|| field(&node, "alias"));
            opaque |= binding.is_some_and(|binding| {
                descendants(&binding)
                    .any(|n| n.kind() == "identifier" && text(&n, source) == receiver)
            });
        }
        if node.kind() == "var_spec" {
            opaque |=
                children(&node).any(|n| n.kind() == "identifier" && text(&n, source) == receiver);
        }
        if let Some(actual_receiver) = run_receiver(&node, source) {
            if !receiver.is_empty() && receiver != "_" && actual_receiver == receiver {
                calls.push(node.clone());
            } else {
                // Could be an alias of the same testing.T; don't guess which
                // runtime sibling names an indirect receiver will introduce.
                opaque = true;
            }
        }
        opaque |= node.kind() == "goto_statement";
        pending.extend(children(&node));
    }
    if calls.is_empty() && !opaque {
        return Ok(target);
    }

    let mut names = std::collections::HashSet::new();
    let unique_literals = calls.iter().all(|call| {
        let mut ancestor = call.parent();
        while let Some(node) = ancestor.filter(|n| *n != body) {
            if node.kind() == "for_statement" {
                return false;
            }
            ancestor = node.parent();
        }
        field(call, "arguments")
            .and_then(|args| children(&args).next())
            .and_then(|arg| literal(&arg, source))
            .is_some_and(|name| names.insert(name.replace(' ', "_")))
    });
    if !opaque && unique_literals {
        if let Some(call) = calls.into_iter().find(|call| contains(call, byte)) {
            let name = literal(
                &children(&field(&call, "arguments").unwrap())
                    .next()
                    .unwrap(),
                source,
            )
            .unwrap();
            target.entry.subtest = Some(name.into());
            if descendants(&call)
                .any(|n| n != call && contains(&n, byte) && run_receiver(&n, source).is_some())
            {
                target.note = Some(format!(
                    "Nested subtest: running enclosing {}/{} with all its subtests.",
                    target.entry.name, name
                ));
            }
            return Ok(target);
        }
    }
    target.note = Some(format!(
        "No unique static subtest at the cursor; running all of {} (including its subtests).",
        target.entry.name
    ));
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_core::tree_sitter::Parser;

    fn selected(marked: &str) -> anyhow::Result<Target> {
        let byte = marked.find("/*cursor*/").unwrap();
        let source = marked.replacen("/*cursor*/", "", 1);
        let mut parser = Parser::new();
        parser
            .set_grammar(
                helix_loader::grammar::get_language("go")
                    .unwrap()
                    .expect("Go grammar required: hx --grammar fetch && hx --grammar build"),
            )
            .unwrap();
        let tree = parser
            .parse(helix_core::Rope::from_str(&source).slice(..), None)
            .unwrap();
        at_cursor(tree.root_node(), &source, byte, "sample_test.go")
    }

    fn test(body: &str) -> Target {
        selected(&format!(
            "package sample\nimport \"testing\"\nfunc TestExample(t *testing.T) {{\n{body}\n}}\n"
        ))
        .unwrap()
    }

    #[test]
    #[ignore = "requires the Go tree-sitter grammar"]
    fn declaration_body_closing_brace_and_multiple_tests() {
        let source = "package sample\nimport \"testing\"\n// åäö\nfunc TestFirst(t *testing.T) { t.Log(`func TestFake(t *testing.T) {}`) }\nfunc TestSecond(t *testing.T) {\n t.Log(\"second\")\n}\n";
        for needle in ["func TestSecond", "TestSecond(", "t.Log(\"second\")", "}\n"] {
            let byte = source.rfind(needle).unwrap();
            let marked = format!("{}/*cursor*/{}", &source[..byte], &source[byte..]);
            let target = selected(&marked).unwrap();
            assert_eq!(target.entry.name, "TestSecond");
            assert!(target.entry.subtest.is_none());
            assert!(target.note.is_none());
        }
    }

    #[test]
    #[ignore = "requires the Go tree-sitter grammar"]
    fn outside_tests_and_invalid_test_signatures_are_rejected() {
        for marked in [
            "/*cursor*/package sample\nimport \"testing\"\nfunc TestYes(t *testing.T) {}",
            "package sample\nimport \"testing\"\n// /*cursor*/func TestFake(t *testing.T) {}",
            "package sample\nimport \"testing\"\nfunc TestYes(t *testing.T) {}/*cursor*/\n",
            "package sample\nimport \"testing\"\nfunc helper(t *testing.T) {/*cursor*/}",
            "package sample\nimport \"testing\"\nfunc Testlower(t *testing.T) {/*cursor*/}",
            "package sample\nimport \"testing\"\nfunc TestWrong(t int) {/*cursor*/}",
            "package sample\nimport \"testing\"\nfunc TestWrong(t *testing.T) int {/*cursor*/return 1}",
            "package sample\nimport \"testing\"\nfunc TestWrong(a,b *testing.T) {/*cursor*/}",
            "package sample\nimport \"testing\"\nfunc (s Suite) TestMethod(t *testing.T) {/*cursor*/}",
        ] {
            assert!(selected(marked).is_err(), "{marked}");
        }
        assert!(test("/*cursor*/t.Log(\"body\")").entry.subtest.is_none());
    }

    #[test]
    #[ignore = "requires the Go tree-sitter grammar"]
    fn static_subtests_use_cursor_containment_and_testing_aliases() {
        for mark in ["/*cursor*/u.Run", "u.Run(/*cursor*/", "u.Log(/*cursor*/"] {
            let source = "package sample\nimport check \"testing\"\nfunc TestAlias(u *check.T) {\n u.Run(\"other\", func(u *check.T) {})\n u.Run(`chosen [case]+`, func(u *check.T) { u.Log(\"hello\") })\n}";
            let needle = mark.replace("/*cursor*/", "");
            let byte = source.rfind(&needle).unwrap();
            let marked = format!(
                "{}{}{}",
                &source[..byte],
                mark,
                &source[byte + needle.len()..]
            );
            let target = selected(&marked).unwrap();
            assert_eq!(target.entry.name, "TestAlias");
            assert_eq!(target.entry.subtest.as_deref(), Some("chosen [case]+"));
            assert!(target.note.is_none());
        }
        assert_eq!(
            selected("package sample\nimport . \"testing\"\nfunc Test(t *T) {/*cursor*/}")
                .unwrap()
                .entry
                .name,
            "Test"
        );
        assert_eq!(
            test("/*cursor*/   t.Run(\"indented\", func(t *testing.T) {})")
                .entry
                .subtest
                .as_deref(),
            Some("indented")
        );
        let between = test("t.Run(\"first\", func(t *testing.T) {})\n/*cursor*/  \n t.Run(\"second\", func(t *testing.T) {})");
        assert!(between.entry.subtest.is_none());
        assert!(between.note.is_some());
    }

    #[test]
    #[ignore = "requires the Go tree-sitter grammar"]
    fn table_rows_shared_loops_and_dynamic_names_run_parent_with_notice() {
        for body in [
            "cases := []struct{name string}{{name: /*cursor*/\"one\"}, {name: \"two\"}}; for _, tc := range cases { t.Run(tc.name, func(t *testing.T) {}) }",
            "for _, name := range []string{\"one\", \"two\"} { t.Run(name, func(t *testing.T) { /*cursor*/t.Log(name) }) }",
            "for range 2 { t.Run(\"same\", func(t *testing.T) { /*cursor*/t.Log(\"body\") }) }",
            "t.Run(fmt.Sprint(42), func(t *testing.T) { /*cursor*/t.Log(\"body\") })",
            "t.Run(\"chosen\", func(t *testing.T) {/*cursor*/}); t.Run(dynamic, func(t *testing.T) {})",
            "run := func() { t.Run(\"hidden\", func(t *testing.T) {/*cursor*/}) }; run()",
        ] {
            let target = test(body);
            assert!(target.entry.subtest.is_none(), "{body}");
            assert!(target.note.as_deref().unwrap().contains("running all of TestExample"));
        }
    }

    #[test]
    #[ignore = "requires the Go tree-sitter grammar"]
    fn ambiguous_names_and_shadowed_receivers_never_choose_a_sibling() {
        for body in [
            "t.Run(\"same\", func(t *testing.T) {}); t.Run(\"same\", func(t *testing.T) {/*cursor*/})",
            "t.Run(\"a b\", func(t *testing.T) {}); t.Run(\"a_b\", func(t *testing.T) {/*cursor*/})",
            "t.Run(\"a/b\", func(t *testing.T) {/*cursor*/})",
            r#"t.Run("a\tb", func(t *testing.T) {/*cursor*/})"#,
            "t.Run(\"\", func(t *testing.T) {/*cursor*/})",
            "t.Run(\"same#01\", func(t *testing.T) {/*cursor*/})",
            "{ t := customRunner{}; t.Run(\"other\", func(t *testing.T) {/*cursor*/}) }",
            "{ var a, t customRunner; _ = a; t.Run(\"other\", func(t *testing.T) {/*cursor*/}) }",
            "switch t := runner.(type) { case customRunner: t.Run(\"other\", func(t *testing.T) {/*cursor*/}) }",
            "u := t; u.Run(name, callback); t.Run(\"chosen\", func(t *testing.T) {/*cursor*/})",
            "again: t.Run(\"same\", func(t *testing.T) {/*cursor*/}); if repeat { goto again }",
        ] {
            let target = test(body);
            assert!(target.entry.subtest.is_none(), "{body}");
            assert!(target.note.is_some(), "{body}");
        }
    }

    #[test]
    #[ignore = "requires the Go tree-sitter grammar"]
    fn nested_subtests_run_known_outer_parent_with_notice() {
        let target = test("t.Run(\"outer\", func(u *testing.T) { u.Run(\"inner\", func(v *testing.T) { /*cursor*/v.Log(\"inside\") }) })");
        assert_eq!(target.entry.subtest.as_deref(), Some("outer"));
        assert!(target
            .note
            .as_deref()
            .unwrap()
            .contains("running enclosing TestExample/outer with all its subtests"));
        let target = test("t.Run(dynamic, func(u *testing.T) { u.Run(\"inner\", func(v *testing.T) { /*cursor*/v.Log(\"inside\") }) })");
        assert!(target.entry.subtest.is_none());
        assert!(target.note.is_some());
    }

    #[test]
    #[ignore = "requires the Go tree-sitter grammar"]
    fn syntax_errors_do_not_launch_a_guessed_test() {
        let error = selected("package sample\nimport \"testing\"\nfunc TestBroken(t *testing.T) { /*cursor*/t.Run( }");
        assert!(error.err().unwrap().to_string().contains("syntax errors"));
    }
}
