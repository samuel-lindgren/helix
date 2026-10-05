//! Test methods found by parsing C# sources. Attributes mark the tests of
//! xUnit, NUnit, MSTest and TUnit alike. What the attributes do at run time
//! (data rows, display names, tests inherited from a base class) is not known
//! here: the unit of selection is the method, or the class with all of them.
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

/// A test method, or a class with test methods as a whole.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::commands) struct DotnetTest {
    /// The declaring type as the runtime names it: `Namespace.Outer+Inner`.
    pub class: String,
    /// None for the class as a whole.
    pub method: Option<String>,
    pub file: PathBuf,
    /// The line of the declared name.
    pub line: usize,
}

impl DotnetTest {
    /// The name `dotnet test` reports and filters by.
    pub fn name(&self) -> String {
        match &self.method {
            Some(method) => format!("{}.{method}", self.class),
            None => self.class.clone(),
        }
    }

    /// The VSTest filter that selects this entry. A method is named exactly,
    /// which includes each of its data rows in all three frameworks. A class
    /// has no name of its own among the results, so its methods are matched.
    pub fn filter(&self) -> String {
        match &self.method {
            Some(method) => format!("FullyQualifiedName={}.{method}", self.class),
            None => format!("FullyQualifiedName~{}.", self.class),
        }
    }

    pub fn namespace(&self) -> &str {
        self.class
            .rsplit_once('.')
            .map_or("", |(namespace, _)| namespace)
    }

    /// The name without its namespace: `Outer+Inner.Method`.
    pub fn short_name(&self) -> String {
        let name = self.name();
        match self.namespace() {
            "" => name,
            namespace => name[namespace.len() + 1..].to_owned(),
        }
    }
}

/// A test with the source range of its whole declaration.
pub(super) struct Declared {
    pub test: DotnetTest,
    pub bytes: Range<usize>,
}

pub(super) fn grammar() -> anyhow::Result<Grammar> {
    static GRAMMAR: OnceCell<Grammar> = OnceCell::new();
    GRAMMAR
        .get_or_try_init(|| {
            helix_loader::grammar::get_language("c-sharp")?.ok_or_else(|| {
                anyhow::anyhow!(
                    "The C# grammar is not installed: hx --grammar fetch && hx --grammar build"
                )
            })
        })
        .copied()
}

/// The tests declared in one source file, each class before its methods.
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
        namespace: String::new(),
        found: Vec::new(),
    };
    walk.declarations(&tree.root_node(), None);
    walk.found
}

/// Every test of the project in this directory. Directories of other projects
/// and build output are left out.
pub(in crate::commands) fn project_tests(dir: &Path) -> anyhow::Result<Vec<DotnetTest>> {
    let grammar = grammar()?;
    let mut files = Vec::new();
    source_files(dir, true, &mut files);
    files.sort();
    let mut tests = Vec::new();
    for file in files {
        let Ok(source) = std::fs::read_to_string(&file) else {
            continue;
        };
        if MAY_DECLARE_TESTS.is_match(&source) {
            tests.extend(
                parse(grammar, &source, &file)
                    .into_iter()
                    .map(|declared| declared.test),
            );
        }
    }
    Ok(tests)
}

/// Cheap to check, and wrong only towards parsing a file without tests.
static MAY_DECLARE_TESTS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?:Fact|Theory|Test|TestCase|TestCaseSource|TestMethod)(?:Attribute)?\s*[\](,]")
        .unwrap()
});

fn source_files(dir: &Path, root: bool, files: &mut Vec<PathBuf>) {
    let entries: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .collect();
    let extension = |entry: &std::fs::DirEntry, wanted: &str| {
        entry.path().extension().is_some_and(|e| e == wanted)
    };
    // Sources below another project file belong to that project.
    if !root && entries.iter().any(|entry| extension(entry, "csproj")) {
        return;
    }
    for entry in entries {
        // Not followed: a link may lead back up the tree.
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if kind.is_dir() {
            if !matches!(name.as_ref(), "bin" | "obj" | "node_modules") && !name.starts_with('.') {
                source_files(&entry.path(), false, files);
            }
        } else if kind.is_file() && extension(&entry, "cs") {
            files.push(entry.path());
        }
    }
}

struct Walk<'a> {
    source: &'a str,
    file: &'a Path,
    namespace: String,
    found: Vec<Declared>,
}

impl Walk<'_> {
    fn text(&self, node: &Node<'_>) -> &str {
        &self.source[node.start_byte() as usize..node.end_byte() as usize]
    }

    fn name(&self, node: &Node<'_>) -> Option<String> {
        // `@class` is the identifier `class`.
        field(node, "name").map(|name| self.text(&name).trim_start_matches('@').to_owned())
    }

    fn line(&self, node: &Node<'_>) -> usize {
        let name = field(node, "name").unwrap_or_else(|| node.clone());
        self.source[..name.start_byte() as usize]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count()
    }

    fn declared(&self, node: &Node<'_>, class: &str, method: Option<String>) -> Declared {
        Declared {
            test: DotnetTest {
                class: class.to_owned(),
                method,
                file: self.file.to_owned(),
                line: self.line(node),
            },
            bytes: node.start_byte() as usize..node.end_byte() as usize,
        }
    }

    /// `class` is the enclosing type, if any.
    fn declarations(&mut self, node: &Node<'_>, class: Option<&str>) {
        for child in node.children().filter(Node::is_named) {
            match child.kind() {
                "namespace_declaration" => {
                    let Some(name) = self.name(&child) else {
                        continue;
                    };
                    let outer = self.namespace.clone();
                    self.namespace = qualify(&outer, '.', &name);
                    if let Some(body) = field(&child, "body") {
                        self.declarations(&body, None);
                    }
                    self.namespace = outer;
                }
                // Applies to the rest of the file.
                "file_scoped_namespace_declaration" => {
                    if let Some(name) = self.name(&child) {
                        self.namespace = name;
                    }
                    self.declarations(&child, None);
                }
                "class_declaration"
                | "struct_declaration"
                | "record_declaration"
                | "record_struct_declaration" => {
                    let (Some(name), Some(body)) = (self.name(&child), field(&child, "body"))
                    else {
                        continue;
                    };
                    let name = match class {
                        Some(outer) => qualify(outer, '+', &name),
                        None => qualify(&self.namespace, '.', &name),
                    };
                    let first = self.found.len();
                    self.declarations(&body, Some(&name));
                    let has_tests = self.found[first..]
                        .iter()
                        .any(|declared| declared.test.class == name);
                    if has_tests {
                        let declared = self.declared(&child, &name, None);
                        self.found.insert(first, declared);
                    }
                }
                "method_declaration" => {
                    if let (Some(class), Some(name)) = (class, self.name(&child)) {
                        if self.is_test(&child) {
                            let declared = self.declared(&child, class, Some(name));
                            self.found.push(declared);
                        }
                    }
                }
                // Declarations inside #if are as real as those outside.
                "declaration_list" => self.declarations(&child, class),
                kind if kind.starts_with("preproc_") => self.declarations(&child, class),
                _ => {}
            }
        }
    }

    fn is_test(&self, method: &Node<'_>) -> bool {
        method
            .children()
            .filter(|list| list.kind() == "attribute_list")
            .flat_map(|list| list.children().collect::<Vec<_>>())
            .filter(|attribute| attribute.kind() == "attribute")
            .filter_map(|attribute| field(&attribute, "name"))
            .any(|name| is_test_attribute(self.text(&name)))
    }
}

fn qualify(scope: &str, separator: char, name: &str) -> String {
    if scope.is_empty() {
        name.to_owned()
    } else {
        format!("{scope}{separator}{name}")
    }
}

/// `name` as written: `Fact`, `Xunit.FactAttribute`, `global::NUnit.Framework.Test`.
/// The suffixes admit the attributes that projects derive from the frameworks'
/// own, such as `SkippableFact` or `RetryTheory`.
fn is_test_attribute(name: &str) -> bool {
    let name = name.rsplit(['.', ':']).next().unwrap_or(name);
    let name = name.split('<').next().unwrap_or(name).trim();
    let name = name.strip_suffix("Attribute").unwrap_or(name);
    matches!(name, "Test" | "TestCase" | "TestCaseSource")
        || ["Fact", "Theory", "TestMethod"]
            .iter()
            .any(|suffix| name.ends_with(suffix))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn names(source: &str) -> Vec<(String, usize)> {
        parse(grammar().unwrap(), source, Path::new("Sample.cs"))
            .into_iter()
            .map(|declared| (declared.test.name(), declared.test.line))
            .collect()
    }

    fn pairs(expected: &[(&str, usize)]) -> Vec<(String, usize)> {
        expected
            .iter()
            .map(|(name, line)| ((*name).to_owned(), *line))
            .collect()
    }

    #[test]
    fn attributes_of_every_framework_and_their_spellings() {
        for (name, expected) in [
            ("Fact", true),
            ("Theory", true),
            ("Test", true),
            ("TestCase", true),
            ("TestCaseSource", true),
            ("TestMethod", true),
            ("DataTestMethod", true),
            ("FactAttribute", true),
            ("Xunit.Fact", true),
            ("global::NUnit.Framework.TestAttribute", true),
            ("SkippableFact", true),
            ("RetryTheory", true),
            ("InlineData", false),
            ("TestFixture", false),
            ("TestClass", false),
            ("SetUp", false),
            ("Trait", false),
            ("Testing", false),
        ] {
            assert_eq!(is_test_attribute(name), expected, "{name}");
        }
        for (source, expected) in [
            ("[Fact]\npublic void A() {}", true),
            ("[Test, Ignore(\"x\")] void A() {}", true),
            ("[Description(\"x\"), TestMethod] void A() {}", true),
            ("[TestCase(1, 2)]\nvoid A(int a, int b) {}", true),
            ("[InlineData(1)] void A() {}", false),
            ("var facts = Tests.Count; [Obsolete] void A() {}", false),
        ] {
            assert_eq!(MAY_DECLARE_TESTS.is_match(source), expected, "{source}");
        }
    }

    #[test]
    fn names_and_filters() {
        let method = DotnetTest {
            class: "Shop.Tests.Calc+Nested".into(),
            method: Some("Adds".into()),
            file: PathBuf::new(),
            line: 0,
        };
        assert_eq!(method.name(), "Shop.Tests.Calc+Nested.Adds");
        assert_eq!(method.short_name(), "Calc+Nested.Adds");
        assert_eq!(method.namespace(), "Shop.Tests");
        assert_eq!(
            method.filter(),
            "FullyQualifiedName=Shop.Tests.Calc+Nested.Adds"
        );
        let class = DotnetTest {
            class: "Calc".into(),
            method: None,
            ..method
        };
        assert_eq!(class.name(), "Calc");
        assert_eq!(class.short_name(), "Calc");
        assert_eq!(class.namespace(), "");
        assert_eq!(class.filter(), "FullyQualifiedName~Calc.");
    }

    #[test]
    #[ignore = "requires the C# tree-sitter grammar"]
    fn file_scoped_namespace_nested_classes_and_non_tests() {
        let source = r#"using Xunit;

namespace Shop.Tests;

public class CalcTests
{
    private readonly int _seed = 1;

    public CalcTests() { }

    [Fact]
    public void Adds() { }

    [Theory]
    [InlineData(1, 2)]
    public async Task AddsTable(int a, int b) { await Task.Yield(); }

    public void Helper() { }

    [Obsolete]
    public void NotATest() { }

    public class Nested
    {
        [Xunit.FactAttribute(Skip = "later")]
        public void @Inner() { }
    }

    private class NoTests
    {
        public void Nothing() { }
    }
}

public static class Helpers
{
    public static int Zero() => 0;
}
"#;
        assert_eq!(
            names(source),
            pairs(&[
                ("Shop.Tests.CalcTests", 4),
                ("Shop.Tests.CalcTests.Adds", 11),
                ("Shop.Tests.CalcTests.AddsTable", 15),
                ("Shop.Tests.CalcTests+Nested", 22),
                ("Shop.Tests.CalcTests+Nested.Inner", 25),
            ])
        );
    }

    #[test]
    #[ignore = "requires the C# tree-sitter grammar"]
    fn block_namespaces_conditional_code_and_other_frameworks() {
        let source = r#"namespace Shop
{
    namespace Tests.Deep
    {
        [TestFixture]
        public class NUnitTests
        {
            [Test, Ignore("not yet")]
            public void Skipped() { }

            [TestCase(1)]
            [TestCase(2)]
            public void Cases(int n) { }
#if DEBUG
            [Test]
            public void OnlyInDebug() { }
#endif
        }
    }

    [TestClass]
    public sealed class MsTests
    {
        [TestMethod]
        [DataRow(1)]
        public void Rows(int n) { }

        [DataTestMethod] public void Old() { }
    }

    // Only a class with tests is listed; this one has a nested class with some.
    public class Outer
    {
        public record Inner
        {
            [Fact] public void InRecord() { }
        }
    }
}

public class Global
{
    [Fact] public void NoNamespace() => Assert.True(true);
}
"#;
        assert_eq!(
            names(source),
            pairs(&[
                ("Shop.Tests.Deep.NUnitTests", 5),
                ("Shop.Tests.Deep.NUnitTests.Skipped", 8),
                ("Shop.Tests.Deep.NUnitTests.Cases", 12),
                ("Shop.Tests.Deep.NUnitTests.OnlyInDebug", 15),
                ("Shop.MsTests", 21),
                ("Shop.MsTests.Rows", 25),
                ("Shop.MsTests.Old", 27),
                ("Shop.Outer+Inner", 33),
                ("Shop.Outer+Inner.InRecord", 35),
                ("Global", 40),
                ("Global.NoNamespace", 42),
            ])
        );
    }

    #[test]
    #[ignore = "requires the C# tree-sitter grammar"]
    fn declarations_cover_their_attributes_and_broken_code_is_tolerated() {
        let source = "class T {\n    [Fact]\n    public void A() { }\n\n    [Fact]\n    public void B() { var x = ; }\n}\n";
        let declared = parse(grammar().unwrap(), source, Path::new("T.cs"));
        let method = |name: &str| {
            declared
                .iter()
                .find(|d| d.test.method.as_deref() == Some(name))
                .unwrap_or_else(|| panic!("{name} not found"))
        };
        assert_eq!(
            &source[method("A").bytes.clone()],
            "[Fact]\n    public void A() { }"
        );
        assert!(source[method("B").bytes.clone()].starts_with("[Fact]"));
        assert_eq!(declared[0].test.method, None);
        assert_eq!(&source[declared[0].bytes.clone()], source.trim_end());
    }

    #[test]
    #[ignore = "requires the C# tree-sitter grammar"]
    fn a_project_is_its_own_sources_only() {
        let project = tempfile::tempdir().unwrap();
        let test = "namespace N;\npublic class T { [Fact] public void A() { } }\n";
        for (path, contents) in [
            ("Tests.csproj", "<Project />"),
            ("T.cs", test),
            ("Sub/Deeper/U.cs", &test.replace("class T", "class U")),
            (
                "Plain.cs",
                "namespace N;\npublic class Plain { public void Fact() { } }\n",
            ),
            ("bin/Debug/Copy.cs", test),
            ("obj/Generated.cs", test),
            (".hidden/H.cs", test),
            ("Other/Other.csproj", "<Project />"),
            ("Other/O.cs", test),
            ("notes.txt", "[Fact]"),
        ] {
            let path = project.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        let tests = project_tests(project.path()).unwrap();
        let found: Vec<_> = tests
            .iter()
            .map(|test| {
                (
                    test.name(),
                    test.file.strip_prefix(project.path()).unwrap().to_owned(),
                )
            })
            .collect();
        assert_eq!(
            found,
            [
                ("N.U".to_owned(), PathBuf::from("Sub/Deeper/U.cs")),
                ("N.U.A".to_owned(), PathBuf::from("Sub/Deeper/U.cs")),
                ("N.T".to_owned(), PathBuf::from("T.cs")),
                ("N.T.A".to_owned(), PathBuf::from("T.cs")),
            ]
        );
    }
}
