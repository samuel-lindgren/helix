//! From a C# type or member to the tests written for it. Nothing in the
//! language ties the two together, only names: the tests of `Calc` are in a
//! class called `CalcTests` or the like, in whichever project of the
//! workspace, and those of its `Add` mention `Add` in their own names.
use std::path::Path;

use helix_core::{
    tree_sitter::{Node, Parser},
    Rope,
};

use super::{
    discovery::{self, field, Below, DotnetTest},
    project_of, workspace_root,
};

/// What tests are looked for: a type, and a member of it when the place is in one.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::commands) struct Subject {
    pub type_name: String,
    pub member: Option<String>,
}

impl std::fmt::Display for Subject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.member {
            Some(member) => write!(f, "{}.{member}", self.type_name),
            None => f.write_str(&self.type_name),
        }
    }
}

/// The tests for what is declared around `byte` of a source file: the test
/// methods that name the member, else the test classes of the type.
pub(in crate::commands) fn corresponding_tests(
    file: &Path,
    source: &str,
    byte: usize,
) -> anyhow::Result<(Subject, Vec<DotnetTest>)> {
    let grammar = discovery::grammar()?;
    // Leading indentation belongs to the declaration on its line.
    let byte = byte
        + source[byte.min(source.len())..]
            .bytes()
            .take_while(|b| matches!(b, b' ' | b'\t'))
            .count();
    let subject = subject_at(grammar, source, byte)
        .ok_or_else(|| anyhow::anyhow!("No C# type or member here to find tests for"))?;
    let root = match project_of(file) {
        Ok(project) => workspace_root(&project),
        Err(_) => file.parent().unwrap_or(file).to_owned(),
    };
    let mut tests = Vec::new();
    for file in discovery::source_files(&root, Below::Workspace) {
        let Ok(source) = std::fs::read_to_string(&file) else {
            continue;
        };
        // The name of a test class contains the name of what it tests.
        if source.contains(&subject.type_name) && discovery::MAY_DECLARE_TESTS.is_match(&source) {
            tests.extend(
                discovery::parse(grammar, &source, &file)
                    .into_iter()
                    .map(|declared| declared.test)
                    .filter(|test| is_test_class_of(simple_name(&test.class), &subject.type_name)),
            );
        }
    }
    let tests = select(&subject, tests);
    Ok((subject, tests))
}

/// Of the entries of the test classes: the methods that name the member, and
/// the classes themselves when none does.
fn select(subject: &Subject, tests: Vec<DotnetTest>) -> Vec<DotnetTest> {
    let naming = |member: &String| {
        tests
            .iter()
            .filter(|test| test.method.as_ref().is_some_and(|name| names(name, member)))
            .cloned()
            .collect::<Vec<_>>()
    };
    match subject.member.as_ref().map(naming) {
        Some(methods) if !methods.is_empty() => methods,
        _ => tests
            .into_iter()
            .filter(|test| test.method.is_none())
            .collect(),
    }
}

/// `Outer+Inner` of `Namespace.Outer+Inner` is named `Inner`.
fn simple_name(class: &str) -> &str {
    class.rsplit(['.', '+']).next().unwrap_or(class)
}

/// Whether a class is, by its name, the tests of a type: `CalcTests`,
/// `Calc_Tests`, `CalcUnitTests`, `TestCalc`.
fn is_test_class_of(class: &str, type_name: &str) -> bool {
    const MARKS: [&str; 12] = [
        "test",
        "tests",
        "unittest",
        "unittests",
        "integrationtest",
        "integrationtests",
        "spec",
        "specs",
        "facts",
        "fixture",
        "should",
        "testfixture",
    ];
    class
        .strip_prefix(type_name)
        .or_else(|| class.strip_suffix(type_name))
        .map(|mark| mark.replace('_', "").to_lowercase())
        .is_some_and(|mark| MARKS.contains(&mark.as_str()))
}

/// Whether the name of a test names a member: `Add` is named by
/// `Add_ReturnsSum`, `AddsNumbers` and `ShouldAddNumbers`, not by `Padded`.
fn names(test: &str, member: &str) -> bool {
    let capitalized = member.starts_with(char::is_uppercase);
    test.match_indices(member)
        .any(|(start, _)| match test[..start].chars().next_back() {
            None | Some('_') => true,
            // A capital starts a word after anything but another capital.
            Some(before) => capitalized && before.is_alphanumeric() && !before.is_uppercase(),
        })
}

/// The innermost type around `byte`, and the member of it that `byte` is in.
fn subject_at(
    grammar: helix_core::tree_sitter::Grammar,
    source: &str,
    byte: usize,
) -> Option<Subject> {
    let mut parser = Parser::new();
    parser.set_grammar(grammar).ok()?;
    let tree = parser.parse(Rope::from_str(source).slice(..), None)?;
    let mut subject = None;
    narrow(&tree.root_node(), source, byte, &mut subject);
    subject
}

fn narrow(node: &Node<'_>, source: &str, byte: usize, subject: &mut Option<Subject>) {
    let name = |node: &Node<'_>| {
        let name = field(node, "name")?;
        let name = &source[name.start_byte() as usize..name.end_byte() as usize];
        Some(name.trim_start_matches('@').to_owned())
    };
    for child in node.children().filter(Node::is_named) {
        if !(child.start_byte() as usize..child.end_byte() as usize).contains(&byte) {
            continue;
        }
        match child.kind() {
            "class_declaration"
            | "struct_declaration"
            | "record_declaration"
            | "record_struct_declaration"
            | "interface_declaration" => {
                if let Some(type_name) = name(&child) {
                    *subject = Some(Subject {
                        type_name,
                        member: None,
                    });
                }
                narrow(&child, source, byte, subject);
            }
            // What is inside a member is the member's.
            "method_declaration" | "property_declaration" => {
                if let Some(subject) = subject {
                    subject.member = name(&child);
                }
            }
            _ => narrow(&child, source, byte, subject),
        }
        return;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classes_are_known_by_their_names() {
        for (class, expected) in [
            ("CalcTests", true),
            ("CalcTest", true),
            ("Calc_Tests", true),
            ("CalcUnitTests", true),
            ("CalcIntegrationTests", true),
            ("CalcSpecs", true),
            ("CalcFacts", true),
            ("CalcShould", true),
            ("TestCalc", true),
            ("Calc", false),
            ("Calculator", false),
            ("CalculatorTests", false),
            ("FastCalcTests", false),
            ("CalcHelpers", false),
            ("Tests", false),
        ] {
            assert_eq!(is_test_class_of(class, "Calc"), expected, "{class}");
        }
        assert_eq!(simple_name("Shop.Tests.Outer+CalcTests"), "CalcTests");
        assert_eq!(simple_name("CalcTests"), "CalcTests");
    }

    #[test]
    fn tests_name_members_at_the_start_of_a_word() {
        for (test, expected) in [
            ("Add", true),
            ("Add_TwoNumbers_ReturnsSum", true),
            ("AddsNumbers", true),
            ("ShouldAddNumbers", true),
            ("Should_Add_Numbers", true),
            ("When2AddIsCalled", true),
            ("Padded", false),
            ("READD", false),
            ("Subtract", false),
        ] {
            assert_eq!(names(test, "Add"), expected, "{test}");
        }
        // A lower-case member only stands out after an underscore.
        assert!(names("parse_handles_empty", "parse"));
        assert!(names("it_can_parse", "parse"));
        assert!(!names("sparse", "parse"));
    }

    fn test(class: &str, method: &str) -> DotnetTest {
        DotnetTest {
            class: class.into(),
            method: (!method.is_empty()).then(|| method.to_owned()),
            file: "/src/Tests.cs".into(),
            line: 0,
        }
    }

    #[test]
    fn methods_that_name_the_member_come_before_their_classes() {
        let tests = || {
            vec![
                test("Shop.Tests.CalcTests", ""),
                test("Shop.Tests.CalcTests", "Add_ReturnsSum"),
                test("Shop.Tests.CalcTests", "Subtract_ReturnsDifference"),
                test("Shop.IntegrationTests.CalcTests", ""),
                test("Shop.IntegrationTests.CalcTests", "AddsOverHttp"),
            ]
        };
        let found = |member: Option<&str>| -> Vec<String> {
            let subject = Subject {
                type_name: "Calc".into(),
                member: member.map(str::to_owned),
            };
            select(&subject, tests())
                .iter()
                .map(DotnetTest::name)
                .collect()
        };
        assert_eq!(
            found(Some("Add")),
            [
                "Shop.Tests.CalcTests.Add_ReturnsSum",
                "Shop.IntegrationTests.CalcTests.AddsOverHttp"
            ]
        );
        let classes = ["Shop.Tests.CalcTests", "Shop.IntegrationTests.CalcTests"];
        assert_eq!(found(Some("Multiply")), classes);
        assert_eq!(found(None), classes);
        let subject = Subject {
            type_name: "Calc".into(),
            member: Some("Add".into()),
        };
        assert_eq!(subject.to_string(), "Calc.Add");
    }

    #[test]
    #[ignore = "requires the C# tree-sitter grammar"]
    fn the_subject_is_the_innermost_type_and_its_member() {
        let source = r#"namespace Shop;

public class Calc
{
    private int _total;

    public int Total => _total;

    public Calc(int start) { _total = start; }

    public int Add(int a, int b)
    {
        int Local(int x) => x + 1;
        return Local(a) + b;
    }

    public record Line(int Number)
    {
        public bool IsFirst() => Number == 1;
    }
}

public interface IStore { void Save(); }
"#;
        let at = |needle: &str| {
            subject_at(
                discovery::grammar().unwrap(),
                source,
                source.find(needle).unwrap(),
            )
            .map(|subject| subject.to_string())
        };
        assert_eq!(at("namespace Shop"), None);
        assert_eq!(at("class Calc"), Some("Calc".into()));
        assert_eq!(at("_total;"), Some("Calc".into()));
        assert_eq!(at("Total =>"), Some("Calc.Total".into()));
        // A constructor is the type's.
        assert_eq!(at("_total = start"), Some("Calc".into()));
        assert_eq!(at("Add(int a"), Some("Calc.Add".into()));
        // A local function is its method's.
        assert_eq!(at("Local(int x)"), Some("Calc.Add".into()));
        assert_eq!(at("record Line"), Some("Line".into()));
        assert_eq!(at("Number == 1"), Some("Line.IsFirst".into()));
        assert_eq!(at("void Save"), Some("IStore.Save".into()));
    }

    #[test]
    #[ignore = "requires the C# tree-sitter grammar"]
    fn tests_are_found_in_every_project_of_the_workspace() {
        let fixture = tempfile::tempdir().unwrap();
        let root = helix_stdx::path::canonicalize(fixture.path());
        let calc = "namespace Shop;\npublic class Calc\n{\n    public int Add(int a, int b) => a + b;\n    public int Negate(int a) => -a;\n}\n";
        for (file, contents) in [
            ("Shop.sln", ""),
            ("src/Shop/Shop.csproj", "<Project />"),
            ("src/Shop/Calc.cs", calc),
            ("src/Shop/Calculator.cs", "namespace Shop;\npublic class Calculator { }\n"),
            ("tests/Shop.Tests/Shop.Tests.csproj", "<Project />"),
            (
                "tests/Shop.Tests/CalcTests.cs",
                "namespace Shop.Tests;\npublic class CalcTests\n{\n    [Fact]\n    public void Add_ReturnsSum() { }\n\n    [Fact]\n    public void Other() { }\n}\n",
            ),
            (
                "tests/Shop.Tests/CalculatorTests.cs",
                "namespace Shop.Tests;\npublic class CalculatorTests\n{\n    [Fact]\n    public void Add_IsNotCalcs() { }\n}\n",
            ),
            (
                "tests/Shop.Slow/Shop.Slow.csproj",
                "<Project />",
            ),
            (
                "tests/Shop.Slow/CalcIntegrationTests.cs",
                "namespace Shop.Slow;\n[TestFixture]\npublic class CalcIntegrationTests\n{\n    [Test]\n    public void AddsOverHttp() { }\n}\n",
            ),
            // Mentions the type and has the name, but declares no tests.
            (
                "tests/Shop.Tests/CalcTestData.cs",
                "namespace Shop.Tests;\npublic class CalcTest { public Calc Make() => new(); }\n",
            ),
            ("tests/Shop.Tests/bin/Debug/CalcTests.cs", "class CalcTests { [Fact] void Add() { } }"),
        ] {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        let source = root.join("src/Shop/Calc.cs");
        let found = |needle: &str| {
            let (subject, tests) =
                corresponding_tests(&source, calc, calc.find(needle).unwrap()).unwrap();
            let tests: Vec<_> = tests
                .into_iter()
                .map(|test| {
                    let file = test.file.strip_prefix(&root).unwrap().to_owned();
                    (test.name(), file.to_string_lossy().into_owned(), test.line)
                })
                .collect();
            (subject.to_string(), tests)
        };
        let unit = "tests/Shop.Tests/CalcTests.cs".to_owned();
        let slow = "tests/Shop.Slow/CalcIntegrationTests.cs".to_owned();
        assert_eq!(
            found("Add("),
            (
                "Calc.Add".to_owned(),
                vec![
                    (
                        "Shop.Slow.CalcIntegrationTests.AddsOverHttp".to_owned(),
                        slow.clone(),
                        5
                    ),
                    (
                        "Shop.Tests.CalcTests.Add_ReturnsSum".to_owned(),
                        unit.clone(),
                        4
                    ),
                ]
            )
        );
        let classes = vec![
            ("Shop.Slow.CalcIntegrationTests".to_owned(), slow, 2),
            ("Shop.Tests.CalcTests".to_owned(), unit, 1),
        ];
        // No test names the member, or there is none: the classes.
        assert_eq!(
            found("Negate("),
            ("Calc.Negate".to_owned(), classes.clone())
        );
        assert_eq!(found("class Calc"), ("Calc".to_owned(), classes));
        // From the indentation of a member's line, as from the member.
        assert_eq!(found("    public int Add(").0, "Calc.Add");
        assert!(corresponding_tests(&source, calc, 0).is_err());
    }
}
