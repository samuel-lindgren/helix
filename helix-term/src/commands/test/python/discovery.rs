//! Python tests found by parsing the sources. unittest runs the methods named
//! `test…` of the classes derived from `TestCase`; pytest runs those too, and
//! also the functions named `test…` of a module and the methods of classes
//! named `Test…`. Parameters, subtests and generated tests exist only at run
//! time: the unit of selection is a function, or a class with all of its tests.
use std::{
    ops::Range,
    path::{Path, PathBuf},
};

use helix_core::{
    regex::Regex,
    tree_sitter::{Grammar, Node, Parser},
    Rope,
};
use once_cell::sync::{Lazy, OnceCell};

use super::super::field;

/// A test function or method, or a class with test methods as a whole.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::commands) struct PythonTest {
    pub file: PathBuf,
    /// The classes the test is declared in, outermost first.
    pub classes: Vec<String>,
    /// None for a class as a whole.
    pub function: Option<String>,
    /// The line of the declared name.
    pub line: usize,
    /// Whether unittest runs it too, not only pytest.
    pub unittest: bool,
}

impl PythonTest {
    /// The name in its module: `CalcTests.test_add`, `CalcTests`, `test_add`.
    pub fn name(&self) -> String {
        self.classes
            .iter()
            .map(String::as_str)
            .chain(self.function.as_deref())
            .collect::<Vec<_>>()
            .join(".")
    }

    /// pytest's node id: `tests/test_calc.py::TestCalc::test_add`, with the
    /// file relative to the directory pytest runs in.
    pub fn node_id(&self, root: &Path) -> String {
        let file = relative(&self.file, root);
        self.classes
            .iter()
            .map(String::as_str)
            .chain(self.function.as_deref())
            .fold(file, |id, name| format!("{id}::{name}"))
    }
}

/// A path as given to a tool that runs in `root`.
pub(super) fn relative(path: &Path, root: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(path) if path.as_os_str().is_empty() => ".".into(),
        Ok(path) => path.to_string_lossy().into_owned(),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

/// A test with the source range of its whole declaration, decorators included.
pub(super) struct Declared {
    pub test: PythonTest,
    pub bytes: Range<usize>,
}

pub(super) fn grammar() -> anyhow::Result<Grammar> {
    static GRAMMAR: OnceCell<Grammar> = OnceCell::new();
    GRAMMAR
        .get_or_try_init(|| {
            helix_loader::grammar::get_language("python")?.ok_or_else(|| {
                anyhow::anyhow!(
                    "The Python grammar is not installed: hx --grammar fetch && hx --grammar build"
                )
            })
        })
        .copied()
}

/// The tests declared in one source file, each class before its tests.
pub(super) fn parse(grammar: Grammar, source: &str, file: &Path) -> Vec<Declared> {
    let mut parser = Parser::new();
    if parser.set_grammar(grammar).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(Rope::from_str(source).slice(..), None) else {
        return Vec::new();
    };
    let mut walk = Walk {
        source,
        file,
        found: Vec::new(),
    };
    walk.statements(&tree.root_node(), &Scope::default());
    walk.found
}

/// Cheap to check, and wrong only towards parsing a file without tests.
pub(super) static MAY_DECLARE_TESTS: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?m)^[ \t]*(?:async[ \t]+)?def[ \t]+test").unwrap());

/// A module written for pytest, which unittest would not run as it is meant to.
pub(super) static USES_PYTEST: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?m)^[ \t]*(?:import|from)[ \t]+pytest\b").unwrap());

/// Whether a file is named as a test module: unittest's `test*.py` or
/// pytest's `*_test.py`.
pub(super) fn is_test_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".py"))
        .is_some_and(|stem| stem.starts_with("test") || stem.ends_with("_test"))
}

/// The files that make a directory the root of a project.
pub(super) const PROJECT_FILES: [&str; 7] = [
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "tox.ini",
    "pytest.ini",
    ".pytest.ini",
    "pyrightconfig.json",
];

/// Whether a directory is the root of a project or of a repository.
fn is_root(dir: &Path) -> bool {
    dir.join(".git").exists() || PROJECT_FILES.iter().any(|file| dir.join(file).is_file())
}

/// The test modules of a project, and whether pytest's `conftest.py` is among
/// its files.
#[derive(Default)]
pub(super) struct ProjectFiles {
    pub tests: Vec<PathBuf>,
    pub conftest: bool,
}

/// The files of the project in `dir`: not those of the projects below it, and
/// not those in virtual environments, caches and build output. Only those of
/// `dir` itself unless `below`.
pub(super) fn project_files(dir: &Path, below: bool) -> ProjectFiles {
    let mut files = ProjectFiles::default();
    collect(dir, below, true, &mut files);
    files.tests.sort();
    files
}

pub(super) fn test_files(dir: &Path, below: bool) -> Vec<PathBuf> {
    project_files(dir, below).tests
}

fn collect(dir: &Path, below: bool, root: bool, files: &mut ProjectFiles) {
    // A virtual environment holds the tests of the packages installed in it.
    if dir.join("pyvenv.cfg").is_file() || !root && is_root(dir) {
        return;
    }
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        // Not followed: a link may lead back up the tree.
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() && below {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let skipped = name.starts_with('.')
                || name.ends_with(".egg-info")
                || matches!(
                    name.as_ref(),
                    "__pycache__" | "node_modules" | "site-packages" | "build" | "dist"
                );
            if !skipped {
                collect(&path, below, false, files);
            }
        } else if kind.is_file() && is_test_file(&path) {
            files.tests.push(path);
        } else if kind.is_file() && entry.file_name() == "conftest.py" {
            files.conftest = true;
        }
    }
}

/// Every test in the test modules of the project in `dir`; see [`project_files`].
pub(super) fn project_tests(dir: &Path, below: bool) -> anyhow::Result<Vec<PythonTest>> {
    let grammar = grammar()?;
    let mut tests = Vec::new();
    for file in test_files(dir, below) {
        tests.extend(file_tests(grammar, &file));
    }
    Ok(tests)
}

/// The tests of one file on disk.
pub(super) fn file_tests(grammar: Grammar, file: &Path) -> Vec<PythonTest> {
    let Ok(source) = std::fs::read_to_string(file) else {
        return Vec::new();
    };
    if !MAY_DECLARE_TESTS.is_match(&source) {
        return Vec::new();
    }
    parse(grammar, &source, file)
        .into_iter()
        .map(|declared| declared.test)
        .collect()
}

/// The name a file is imported by, and the directory it is imported from:
/// the first directory up from the file that is not a package.
pub(in crate::commands) fn module_of(file: &Path) -> Option<(PathBuf, String)> {
    let mut parts = vec![file.file_stem()?.to_str()?.to_owned()];
    let mut dir = file.parent()?;
    while dir.join("__init__.py").is_file() {
        parts.push(dir.file_name()?.to_str()?.to_owned());
        dir = dir.parent()?;
    }
    parts.reverse();
    Some((dir.to_owned(), parts.join(".")))
}

/// The directory that the modules of `dir` are imported from: the first
/// directory up from it that is not a package.
pub(super) fn import_root(dir: &Path) -> PathBuf {
    dir.ancestors()
        .find(|dir| !dir.join("__init__.py").is_file())
        .unwrap_or(dir)
        .to_owned()
}

/// unittest's loader and pytest both take functions and methods by this prefix.
fn is_test_name(name: &str) -> bool {
    name.starts_with("test")
}

struct Walk<'a> {
    source: &'a str,
    file: &'a Path,
    found: Vec<Declared>,
}

/// The classes around a statement.
#[derive(Default)]
struct Scope {
    /// Outermost first.
    classes: Vec<String>,
    /// Whether unittest runs the methods of the innermost class.
    unittest: bool,
    /// Whether pytest looks for classes nested in the innermost class.
    nests: bool,
}

impl Walk<'_> {
    fn text(&self, node: &Node<'_>) -> &str {
        &self.source[node.start_byte() as usize..node.end_byte() as usize]
    }

    fn name(&self, node: &Node<'_>) -> Option<String> {
        field(node, "name").map(|name| self.text(&name).to_owned())
    }

    fn line(&self, node: &Node<'_>) -> usize {
        let name = field(node, "name").unwrap_or_else(|| node.clone());
        self.source[..name.start_byte() as usize]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count()
    }

    fn declared(
        &self,
        node: &Node<'_>,
        whole: &Node<'_>,
        classes: &[String],
        function: Option<String>,
        unittest: bool,
    ) -> Declared {
        Declared {
            test: PythonTest {
                file: self.file.to_owned(),
                classes: classes.to_vec(),
                function,
                line: self.line(node),
                unittest,
            },
            bytes: whole.start_byte() as usize..whole.end_byte() as usize,
        }
    }

    /// The statements of a module or of a class body. Definitions inside
    /// `if`, `try` and `with` blocks are as real as the others; those inside
    /// functions are not tests.
    fn statements(&mut self, node: &Node<'_>, scope: &Scope) {
        for child in node.children().filter(Node::is_named) {
            self.statement(&child, &child, scope);
        }
    }

    /// `whole` is the statement: `node` with its decorators.
    fn statement(&mut self, node: &Node<'_>, whole: &Node<'_>, scope: &Scope) {
        match node.kind() {
            "decorated_definition" => {
                if let Some(definition) = field(node, "definition") {
                    self.statement(&definition, node, scope);
                }
            }
            "class_definition" if scope.classes.is_empty() || scope.nests => {
                self.class(node, whole, scope)
            }
            "function_definition" => {
                if let Some(name) = self.name(node).filter(|name| is_test_name(name)) {
                    // A function of a module is a test of pytest's alone.
                    let declared =
                        self.declared(node, whole, &scope.classes, Some(name), scope.unittest);
                    self.found.push(declared);
                }
            }
            "if_statement" | "elif_clause" | "else_clause" | "try_statement" | "except_clause"
            | "finally_clause" | "with_statement" | "block" => self.statements(node, scope),
            _ => {}
        }
    }

    fn class(&mut self, node: &Node<'_>, whole: &Node<'_>, outer: &Scope) {
        let (Some(name), Some(body)) = (self.name(node), field(node, "body")) else {
            return;
        };
        // unittest takes the classes of a module that derive from TestCase,
        // which pytest runs as well. pytest also takes the classes named
        // Test… that have no __init__, and those nested in them.
        let derived = self.derives(node);
        let pytest_class = name.starts_with("Test") && !self.defines_init(&body);
        if !(derived || pytest_class) {
            return;
        }
        let mut classes = outer.classes.clone();
        classes.push(name);
        let scope = Scope {
            unittest: derived && outer.classes.is_empty(),
            nests: !derived,
            classes,
        };
        let first = self.found.len();
        self.statements(&body, &scope);
        if self.found.len() > first {
            let declared = self.declared(node, whole, &scope.classes, None, scope.unittest);
            self.found.insert(first, declared);
        }
    }

    /// Whether a class names a base class, which is what makes a TestCase:
    /// `unittest.TestCase`, or a base of the project's that derives from it.
    fn derives(&self, class: &Node<'_>) -> bool {
        let Some(bases) = field(class, "superclasses") else {
            return false;
        };
        bases
            .children()
            .filter(Node::is_named)
            .filter(|base| !matches!(base.kind(), "keyword_argument" | "comment"))
            .any(|base| self.text(&base) != "object")
    }

    fn defines_init(&self, body: &Node<'_>) -> bool {
        body.children().filter(Node::is_named).any(|statement| {
            let definition = match statement.kind() {
                "decorated_definition" => field(&statement, "definition"),
                _ => Some(statement),
            };
            definition.is_some_and(|definition| {
                definition.kind() == "function_definition"
                    && self.name(&definition).as_deref() == Some("__init__")
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tests of a source, as `(name, line, unittest)`.
    fn found(source: &str) -> Vec<(String, usize, bool)> {
        parse(grammar().unwrap(), source, Path::new("/src/test_sample.py"))
            .into_iter()
            .map(|declared| {
                let test = declared.test;
                (test.name(), test.line, test.unittest)
            })
            .collect()
    }

    fn expect(expected: &[(&str, usize, bool)]) -> Vec<(String, usize, bool)> {
        expected
            .iter()
            .map(|(name, line, unittest)| ((*name).to_owned(), *line, *unittest))
            .collect()
    }

    #[test]
    fn unittest_cases_and_their_methods() {
        let source = r#"import unittest
from base import TempTest


class CalcTests(unittest.TestCase):
    def setUp(self):
        self.calc = 1

    def test_add(self):
        pass

    @unittest.skip("later")
    def test_skipped(self):
        pass

    async def test_async(self):
        pass

    def testCamel(self):
        pass

    def helper_test(self):
        pass


class GuideTests(TempTest, metaclass=Meta):
    def test_guide(self):
        def test_inner():
            pass


class Helpers(unittest.TestCase):
    def check(self):
        pass


class Plain:
    def test_not_a_case(self):
        pass


class Base(object):
    def test_object_only(self):
        pass
"#;
        assert_eq!(
            found(source),
            expect(&[
                ("CalcTests", 4, true),
                ("CalcTests.test_add", 8, true),
                ("CalcTests.test_skipped", 12, true),
                ("CalcTests.test_async", 15, true),
                ("CalcTests.testCamel", 18, true),
                ("GuideTests", 25, true),
                ("GuideTests.test_guide", 26, true),
            ])
        );
    }

    #[test]
    fn pytest_functions_classes_and_nesting() {
        let source = r#"import pytest


def test_add():
    assert True


@pytest.mark.parametrize("a", [1, 2])
def test_param(a):
    pass


def helper():
    pass


class TestCalc:
    def test_method(self):
        pass

    class TestNested:
        def test_inner(self):
            pass

    class Helper:
        def test_hidden(self):
            pass


class TestWithInit:
    def __init__(self):
        pass

    def test_never(self):
        pass


class CalcTests(unittest.TestCase):
    class TestInCase:
        def test_nested_in_case(self):
            pass

    def test_case(self):
        pass


if sys.platform == "linux":
    def test_linux():
        pass
else:
    def test_other():
        pass

try:
    import numpy
except ImportError:
    def test_without_numpy():
        pass
"#;
        assert_eq!(
            found(source),
            expect(&[
                ("test_add", 3, false),
                ("test_param", 8, false),
                ("TestCalc", 16, false),
                ("TestCalc.test_method", 17, false),
                ("TestCalc.TestNested", 20, false),
                ("TestCalc.TestNested.test_inner", 21, false),
                ("CalcTests", 37, true),
                ("CalcTests.test_case", 42, true),
                ("test_linux", 47, false),
                ("test_other", 50, false),
                ("test_without_numpy", 56, false),
            ])
        );
    }

    #[test]
    fn a_declaration_spans_its_decorators() {
        let source = "class TestA:\n    @mark\n    def test_b(self):\n        pass\n";
        let declared = parse(grammar().unwrap(), source, Path::new("test_a.py"));
        let ranges: Vec<_> = declared
            .iter()
            .map(|d| (d.test.name(), &source[d.bytes.clone()]))
            .collect();
        assert_eq!(
            ranges,
            [
                ("TestA".to_owned(), source.trim_end()),
                (
                    "TestA.test_b".to_owned(),
                    "@mark\n    def test_b(self):\n        pass"
                ),
            ]
        );
    }

    #[test]
    fn names_and_node_ids() {
        let test = PythonTest {
            file: "/src/tests/test_calc.py".into(),
            classes: vec!["TestCalc".into(), "TestNested".into()],
            function: Some("test_add".into()),
            line: 3,
            unittest: false,
        };
        assert_eq!(test.name(), "TestCalc.TestNested.test_add");
        assert_eq!(
            test.node_id(Path::new("/src")),
            "tests/test_calc.py::TestCalc::TestNested::test_add"
        );
        let module_function = PythonTest {
            classes: Vec::new(),
            ..test.clone()
        };
        assert_eq!(module_function.name(), "test_add");
        assert_eq!(
            module_function.node_id(Path::new("/elsewhere")),
            "/src/tests/test_calc.py::test_add"
        );
        let class = PythonTest {
            function: None,
            ..test
        };
        assert_eq!(class.name(), "TestCalc.TestNested");
        assert_eq!(relative(Path::new("/src"), Path::new("/src")), ".");
        assert_eq!(
            relative(Path::new("/src/tests"), Path::new("/src")),
            "tests"
        );
    }

    fn tree(files: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for file in files {
            let path = dir.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        dir
    }

    #[test]
    fn modules_import_from_the_first_directory_that_is_no_package() {
        let dir = tree(&[
            "src/shop/__init__.py",
            "src/shop/tests/__init__.py",
            "src/shop/tests/test_cart.py",
            "tests/test_flat.py",
        ]);
        let root = dir.path();
        assert_eq!(
            module_of(&root.join("src/shop/tests/test_cart.py")),
            Some((root.join("src"), "shop.tests.test_cart".to_owned()))
        );
        assert_eq!(
            module_of(&root.join("tests/test_flat.py")),
            Some((root.join("tests"), "test_flat".to_owned()))
        );
        assert_eq!(import_root(&root.join("src/shop/tests")), root.join("src"));
        assert_eq!(import_root(&root.join("tests")), root.join("tests"));
    }

    #[test]
    fn test_modules_outside_environments_caches_and_builds() {
        let dir = tree(&[
            "pyproject.toml",
            "test_root.py",
            "services/api/pyproject.toml",
            "services/api/tests/test_api.py",
            "vendor/lib/.git",
            "vendor/lib/test_vendored.py",
            "tests/test_a.py",
            "tests/a_test.py",
            "tests/testing.py",
            "tests/helpers.py",
            "tests/test.txt",
            "pkg/tests/test_deep.py",
            ".venv/pyvenv.cfg",
            ".venv/lib/python3.12/site-packages/numpy/tests/test_np.py",
            "venv/pyvenv.cfg",
            "venv/lib/test_venv.py",
            ".hidden/test_hidden.py",
            "node_modules/x/test_node.py",
            "build/lib/test_built.py",
            "pkg.egg-info/test_egg.py",
            "tests/__pycache__/test_a.cpython-312.py",
        ]);
        let files = project_files(dir.path(), true);
        let found: Vec<_> = files
            .tests
            .iter()
            .map(|file| relative(file, dir.path()))
            .collect();
        assert_eq!(
            found,
            [
                "pkg/tests/test_deep.py",
                "test_root.py",
                "tests/a_test.py",
                "tests/test_a.py",
                "tests/testing.py",
            ]
        );
        assert!(!files.conftest);
        // Outside of a project only the directory itself counts: it may be
        // the home directory.
        assert_eq!(
            test_files(dir.path(), false),
            [dir.path().join("test_root.py")]
        );
        std::fs::write(dir.path().join("pkg/tests/conftest.py"), "").unwrap();
        assert!(project_files(dir.path(), true).conftest);
        assert!(!project_files(dir.path(), false).conftest);
    }

    #[test]
    fn quick_checks_err_towards_parsing() {
        assert!(MAY_DECLARE_TESTS.is_match("class A:\n    def test_x(self): pass\n"));
        assert!(MAY_DECLARE_TESTS.is_match("async  def testing(): pass"));
        assert!(!MAY_DECLARE_TESTS.is_match("def check(): pass\n# def test later\n"));
        assert!(USES_PYTEST.is_match("import os\nimport pytest\n"));
        assert!(USES_PYTEST.is_match("    from pytest import raises\n"));
        assert!(!USES_PYTEST.is_match("import pytest_asyncio_helpers_not\n"));
        assert!(!USES_PYTEST.is_match("# we do not import pytest here\n"));
    }
}
