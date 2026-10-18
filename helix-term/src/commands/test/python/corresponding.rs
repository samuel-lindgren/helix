//! From Python code to the tests written for it. Only names tie the two
//! together: the tests of `calc.py` are in `test_calc.py` or `calc_test.py`,
//! those of a class `Calc` in classes called `TestCalc` or `CalcTests`, and
//! those of a function or method `add` mention `add` in their own names.
use std::path::{Path, PathBuf};

use helix_core::{
    tree_sitter::{Grammar, Node, Parser},
    Rope,
};

use super::{
    super::field,
    discovery::{self, PythonTest},
    Project,
};

/// What tests are looked for: those of a module, of a class or function in
/// it, or of a method of the class.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::commands) struct Subject {
    /// The module's own name, without its package: `calc`.
    pub module: String,
    pub class: Option<String>,
    pub function: Option<String>,
}

impl std::fmt::Display for Subject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<_> = [
            Some(&self.module),
            self.class.as_ref(),
            self.function.as_ref(),
        ]
        .into_iter()
        .flatten()
        .map(String::as_str)
        .collect();
        f.write_str(&names.join("."))
    }
}

/// The tests for what is declared around `byte` of a source file: the tests
/// that name the function or method, else the test classes of the class,
/// else the test modules of the module. Each is a file and a line.
pub(in crate::commands) fn corresponding_tests(
    file: &Path,
    source: &str,
    byte: usize,
) -> anyhow::Result<(Subject, Vec<(PathBuf, usize)>)> {
    let grammar = discovery::grammar()?;
    // Leading indentation belongs to the declaration on its line.
    let byte = byte
        + source[byte.min(source.len())..]
            .bytes()
            .take_while(|b| matches!(b, b' ' | b'\t'))
            .count();
    let (class, function) = declaration_at(grammar, source, byte);
    let subject = Subject {
        module: module_name(file)
            .ok_or_else(|| anyhow::anyhow!("No Python module to find tests for"))?,
        class,
        function,
    };
    let (root, below) = Project::root(file.parent().unwrap_or(Path::new(".")));
    let files = discovery::test_files(&root, below);
    let tests: Vec<_> = files
        .iter()
        .flat_map(|file| discovery::file_tests(grammar, file))
        .collect();
    let found = select(&subject, &tests, &files);
    Ok((subject, found))
}

/// The name a module has of its own: the file's, or the package's for its
/// `__init__.py`.
fn module_name(file: &Path) -> Option<String> {
    let stem = file.file_stem()?.to_str()?;
    if stem == "__init__" {
        return Some(file.parent()?.file_name()?.to_str()?.to_owned());
    }
    Some(stem.to_owned())
}

/// The innermost class and the outermost function in it around `byte`: a
/// function nested in another one is part of it.
fn declaration_at(grammar: Grammar, source: &str, byte: usize) -> (Option<String>, Option<String>) {
    let mut parser = Parser::new();
    if parser.set_grammar(grammar).is_err() {
        return (None, None);
    }
    let Some(tree) = parser.parse(Rope::from_str(source).slice(..), None) else {
        return (None, None);
    };
    let (mut class, mut function) = (None, None);
    narrow(&tree.root_node(), source, byte, &mut class, &mut function);
    (class, function)
}

fn narrow(
    node: &Node<'_>,
    source: &str,
    byte: usize,
    class: &mut Option<String>,
    function: &mut Option<String>,
) {
    let name = |node: &Node<'_>| {
        let name = field(node, "name")?;
        Some(source[name.start_byte() as usize..name.end_byte() as usize].to_owned())
    };
    for child in node.children().filter(Node::is_named) {
        // A definition ends before the line break of its last line, where
        // the cursor can be too.
        if !(child.start_byte() as usize..=child.end_byte() as usize).contains(&byte) {
            continue;
        }
        match child.kind() {
            "class_definition" if function.is_none() => {
                *class = name(&child);
                narrow(&child, source, byte, class, function);
            }
            "function_definition" => {
                if function.is_none() {
                    *function = name(&child);
                }
                narrow(&child, source, byte, class, function);
            }
            _ => narrow(&child, source, byte, class, function),
        }
        return;
    }
}

/// Of the project's tests, those of the subject: the tests that name its
/// function or method, else the test classes of its class, else its test
/// modules. A function of a module is looked for in the module's tests
/// before the project's.
fn select(subject: &Subject, tests: &[PythonTest], files: &[PathBuf]) -> Vec<(PathBuf, usize)> {
    // Test modules named after part of the module count when none is named
    // after all of it: `test_storage_paths.py` for `storage.py`.
    let mut modules: Vec<_> = files
        .iter()
        .filter(|file| tests_module(file, &subject.module))
        .collect();
    if modules.is_empty() {
        modules = files
            .iter()
            .filter(|file| tests_part_of(file, &subject.module))
            .collect();
    }
    let in_modules = |test: &&PythonTest| modules.contains(&&test.file);
    let naming = |member: &str, test: &&PythonTest| {
        test.function
            .as_deref()
            .is_some_and(|function| names(function, member))
    };
    let place = |test: &PythonTest| (test.file.clone(), test.line);
    let modules_themselves = || modules.iter().map(|file| ((*file).clone(), 0)).collect();
    let Some(class) = &subject.class else {
        let Some(function) = &subject.function else {
            return modules_themselves();
        };
        let near: Vec<_> = tests
            .iter()
            .filter(in_modules)
            .filter(|test| naming(function, test))
            .map(place)
            .collect();
        if !near.is_empty() {
            return near;
        }
        // Tests that name it exactly, wherever they are.
        let named: Vec<_> = tests
            .iter()
            .filter(|test| {
                test.function
                    .as_deref()
                    .is_some_and(|name| words(name).get(1..) == Some(&words(function)[..]))
            })
            .map(place)
            .collect();
        return if named.is_empty() {
            modules_themselves()
        } else {
            named
        };
    };
    let of_class = |test: &&PythonTest| {
        test.classes
            .last()
            .is_some_and(|name| is_test_class_of(name, class))
    };
    if let Some(member) = &subject.function {
        let methods: Vec<_> = tests
            .iter()
            .filter(of_class)
            .filter(|test| naming(member, test))
            .map(place)
            .collect();
        if !methods.is_empty() {
            return methods;
        }
    }
    let classes: Vec<_> = tests
        .iter()
        .filter(of_class)
        .filter(|test| test.function.is_none())
        .map(place)
        .collect();
    if !classes.is_empty() {
        return classes;
    }
    // A module tested without classes for its own: the tests of the module
    // that name the member or the class.
    let member = subject.function.as_deref().unwrap_or(class);
    let near: Vec<_> = tests
        .iter()
        .filter(in_modules)
        .filter(|test| naming(member, test))
        .map(place)
        .collect();
    if near.is_empty() {
        modules_themselves()
    } else {
        near
    }
}

/// Whether a file is named as the tests of a module: `test_calc.py`,
/// `tests_calc.py`, `calc_test.py` or `calc_tests.py` for `calc`.
fn tests_module(file: &Path, module: &str) -> bool {
    let Some(stem) = file.file_stem().and_then(|stem| stem.to_str()) else {
        return false;
    };
    let named = stem
        .strip_prefix("test_")
        .or_else(|| stem.strip_prefix("tests_"))
        .or_else(|| stem.strip_suffix("_test"))
        .or_else(|| stem.strip_suffix("_tests"));
    named == Some(module)
}

/// Whether a file is named as the tests of a part of a module:
/// `test_storage_paths.py` for `storage`.
fn tests_part_of(file: &Path, module: &str) -> bool {
    file.file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| stem.strip_prefix("test_"))
        .and_then(|rest| rest.strip_prefix(module))
        .is_some_and(|rest| rest.starts_with('_'))
}

/// Whether a class is, by its name, the tests of a class: `TestCalc`,
/// `CalcTests`, `CalcTestCase`, `Test_Calc`.
fn is_test_class_of(test: &str, class: &str) -> bool {
    const MARKS: [&str; 9] = [
        "test",
        "tests",
        "testcase",
        "unittest",
        "unittests",
        "integrationtest",
        "integrationtests",
        "spec",
        "specs",
    ];
    test.strip_prefix(class)
        .or_else(|| test.strip_suffix(class))
        .map(|mark| mark.replace('_', "").to_lowercase())
        .is_some_and(|mark| MARKS.contains(&mark.as_str()))
}

/// The words of a name in either case: `test_add_twice` and `testAddTwice`
/// are `test`, `add` and `twice`.
fn words(name: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut previous: Option<char> = None;
    for c in name.chars() {
        let starts = c == '_'
            || c.is_uppercase() && previous.is_some_and(|p| p.is_lowercase() || p.is_ascii_digit());
        if starts && !word.is_empty() {
            words.push(std::mem::take(&mut word));
        }
        if c != '_' {
            word.extend(c.to_lowercase());
        }
        previous = Some(c);
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

/// Whether the name of a test names a function: `test_add`, `test_add_twice`,
/// `test_calc_add` and `testAdd` name `add`; `test_address` does not.
fn names(test: &str, function: &str) -> bool {
    let (test, function) = (words(test), words(function));
    !function.is_empty()
        && test.len() > function.len()
        && test[1..]
            .windows(function.len())
            .any(|window| window == function.as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_words_in_either_case() {
        assert_eq!(words("test_add_twice"), ["test", "add", "twice"]);
        assert_eq!(words("testAddTwice"), ["test", "add", "twice"]);
        assert_eq!(
            words("test_parseHTTP2Header"),
            ["test", "parse", "http2", "header"]
        );
        assert_eq!(words("_private"), ["private"]);
        for (test, expected) in [
            ("test_add", true),
            ("test_add_twice", true),
            ("test_calc_add", true),
            ("test_can_add_numbers", true),
            ("testAdd", true),
            ("testAddTwice", true),
            ("test_address", false),
            ("test_adds", false),
            ("test", false),
            ("add", false),
        ] {
            assert_eq!(names(test, "add"), expected, "{test}");
        }
        assert!(names("test_parse_order_rejects_empty", "parse_order"));
        assert!(names("testParseOrder", "parse_order"));
        assert!(names("test_parse_order", "parseOrder"));
        assert!(!names("test_parse", "parse_order"));
    }

    #[test]
    fn test_classes_and_modules_are_known_by_their_names() {
        for (test, expected) in [
            ("TestCalc", true),
            ("CalcTests", true),
            ("CalcTest", true),
            ("CalcTestCase", true),
            ("Test_Calc", true),
            ("CalcUnitTests", true),
            ("TestCalculator", false),
            ("FastCalcTests", false),
            ("Calc", false),
            ("Tests", false),
        ] {
            assert_eq!(is_test_class_of(test, "Calc"), expected, "{test}");
        }
        for (file, expected) in [
            ("tests/test_calc.py", true),
            ("calc_test.py", true),
            ("tests_calc.py", true),
            ("calc_tests.py", true),
            ("test_calc_parser.py", false),
            ("testcalc.py", false),
            ("calc.py", false),
        ] {
            assert_eq!(tests_module(Path::new(file), "calc"), expected, "{file}");
        }
        assert!(tests_part_of(Path::new("tests/test_calc_paths.py"), "calc"));
        assert!(!tests_part_of(
            Path::new("tests/test_calculator.py"),
            "calc"
        ));
        assert!(!tests_part_of(Path::new("tests/test_calc.py"), "calc"));
        assert_eq!(
            module_name(Path::new("/src/shop/calc.py")),
            Some("calc".into())
        );
        assert_eq!(
            module_name(Path::new("/src/shop/__init__.py")),
            Some("shop".into())
        );
    }

    #[test]
    fn the_declaration_is_the_innermost_class_and_its_function() {
        let source = "import os\n\n\nclass Calc:\n    rate = 1\n\n    def add(self, a, b):\n        def inner(x):\n            return x\n        return inner(a) + b\n\n    class Line:\n        def first(self):\n            pass\n\n\ndef total(prices):\n    return sum(prices)\n";
        let at = |needle: &str| {
            declaration_at(
                discovery::grammar().unwrap(),
                source,
                source.find(needle).unwrap(),
            )
        };
        let owned = |class: Option<&str>, function: Option<&str>| {
            (class.map(str::to_owned), function.map(str::to_owned))
        };
        assert_eq!(at("import os"), owned(None, None));
        assert_eq!(at("class Calc"), owned(Some("Calc"), None));
        assert_eq!(at("rate = 1"), owned(Some("Calc"), None));
        assert_eq!(at("add(self"), owned(Some("Calc"), Some("add")));
        // A nested function is its function's.
        assert_eq!(at("return x"), owned(Some("Calc"), Some("add")));
        assert_eq!(at("pass"), owned(Some("Line"), Some("first")));
        assert_eq!(at("sum(prices)"), owned(None, Some("total")));
        // The end of a definition's last line is the definition's.
        assert_eq!(at("\n\n\ndef total"), owned(Some("Line"), Some("first")));
    }

    #[test]
    fn tests_are_looked_for_by_class_then_module() {
        let fixture = tempfile::tempdir().unwrap();
        let root = helix_stdx::path::canonicalize(fixture.path());
        let calc = "class Calc:\n    def add(self, a, b):\n        return a + b\n\n    def negate(self, a):\n        return -a\n\n\ndef total(prices):\n    return sum(prices)\n\n\ndef unused():\n    pass\n";
        for (file, contents) in [
            ("pyproject.toml", ""),
            ("src/shop/__init__.py", ""),
            ("src/shop/calc.py", calc),
            (
                "tests/test_calc.py",
                "import unittest\n\n\nclass CalcTests(unittest.TestCase):\n    def test_add(self):\n        pass\n\n    def test_add_negative(self):\n        pass\n\n\nclass TotalTests(unittest.TestCase):\n    def test_total_of_nothing(self):\n        pass\n",
            ),
            (
                "tests/integration/test_api.py",
                "class TestCalc:\n    def test_add_over_http(self):\n        pass\n\n\ndef test_total():\n    pass\n",
            ),
        ] {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        let source = root.join("src/shop/calc.py");
        let unit = root.join("tests/test_calc.py");
        let api = root.join("tests/integration/test_api.py");
        let found = |needle: &str| {
            let (subject, found) =
                corresponding_tests(&source, calc, calc.find(needle).unwrap()).unwrap();
            (subject.to_string(), found)
        };
        // A method: the tests that name it, in every test class of its class.
        assert_eq!(
            found("add(self"),
            (
                "calc.Calc.add".into(),
                vec![(api.clone(), 1), (unit.clone(), 4), (unit.clone(), 7)]
            )
        );
        // A method no test names: the test classes.
        assert_eq!(
            found("negate"),
            (
                "calc.Calc.negate".into(),
                vec![(api.clone(), 0), (unit.clone(), 3)]
            )
        );
        // A function of the module: the module's tests that name it first.
        assert_eq!(
            found("sum(prices)"),
            ("calc.total".into(), vec![(unit.clone(), 12)])
        );
        // Nothing names it, nor does any test elsewhere: the test module.
        assert_eq!(
            found("pass"),
            ("calc.unused".into(), vec![(unit.clone(), 0)])
        );
        assert_eq!(
            found("class Calc"),
            (
                "calc.Calc".into(),
                vec![(api.clone(), 0), (unit.clone(), 3)]
            )
        );
        // Without a test module named after the module, those named after a
        // part of it.
        let paths = root.join("tests/test_calc_paths.py");
        std::fs::rename(&unit, &paths).unwrap();
        assert_eq!(found("pass"), ("calc.unused".into(), vec![(paths, 0)]));
    }
}
