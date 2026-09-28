//! Map runtime test names from `go test` output (`TestTable/case_one#01`) back
//! to the source lines that name them. Textual, like test discovery: gofmt
//! layout is assumed. Names built at runtime have no source literal; those
//! resolve to the closest enclosing test or suite method instead of a guess.
use std::path::{Path, PathBuf};

use helix_core::regex::Regex;
use once_cell::sync::Lazy;

use super::SourceLocation;

pub(super) struct Resolution {
    /// Candidate lines, best first. More than one means the name is ambiguous.
    pub locations: Vec<SourceLocation>,
    /// The part of the runtime name the locations belong to. Shorter than the
    /// requested name when a subtest name has no source literal.
    pub shown: String,
}

/// The `_test.go` files of one package, read once for a batch of lookups.
pub(super) struct Package {
    files: Vec<File>,
}

struct File {
    path: PathBuf,
    lines: Vec<String>,
    functions: Vec<Function>,
}

struct Function {
    name: String,
    receiver: Option<String>,
    /// From the declaration line through its closing brace.
    lines: std::ops::Range<usize>,
}

static FUNCTION: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^func(?:\s+|\s*\(\s*(?:\w+\s+)?\*?\s*(\w+)[^)]*\)\s*)(\w+)\s*[\[(]").unwrap()
});
static ORDINAL: Lazy<Regex> = Lazy::new(|| Regex::new(r"#([0-9]{2,})$").unwrap());
static NAME_KEY: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:name|desc|description|title|scenario|label|case)\s*:$").unwrap()
});

impl Package {
    pub(super) fn read(dir: &Path) -> Self {
        let mut paths: Vec<_> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with("_test.go"))
            })
            .collect();
        paths.sort();
        let files = paths
            .into_iter()
            .filter_map(|path| {
                let source = std::fs::read_to_string(&path).ok()?;
                Some(File::parse(helix_stdx::path::canonicalize(path), &source))
            })
            .collect();
        Self { files }
    }

    pub(super) fn resolve(&self, name: &str) -> Option<Resolution> {
        let mut segments: Vec<&str> = name.split('/').collect();
        let (mut file, mut scope) = self.function(segments[0], None)?;
        let mut shown = segments.remove(0).to_owned();
        // testify suites run their methods as subtests: TestSuite/TestMethod.
        if let Some((method_file, method)) = segments
            .first()
            .and_then(|segment| self.suite_method(file, scope, segment))
        {
            (file, scope) = (method_file, method);
            shown = format!("{shown}/{}", segments.remove(0));
        }
        // Prefer the most specific case whose name is written in the source.
        for depth in (0..segments.len()).rev() {
            let mut locations = self.named_lines(file, scope, segments[depth]);
            if !locations.is_empty() {
                // Go appends #01, #02… to repeated names in execution order,
                // so the numbered occurrence is the likeliest row.
                let ordinal = ORDINAL
                    .captures(segments[depth])
                    .and_then(|c| c[1].parse::<usize>().ok())
                    .unwrap_or(0);
                if ordinal < locations.len() {
                    let chosen = locations.remove(ordinal);
                    locations.insert(0, chosen);
                }
                for segment in &segments[..=depth] {
                    shown = format!("{shown}/{segment}");
                }
                return Some(Resolution { locations, shown });
            }
        }
        let file = &self.files[file];
        Some(Resolution {
            locations: vec![file.location(scope.lines.start)],
            shown,
        })
    }

    /// A top-level function (`receiver: None`) or a method with this name.
    fn function(&self, name: &str, receiver: Option<&str>) -> Option<(usize, &Function)> {
        self.files.iter().enumerate().find_map(|(index, file)| {
            file.functions
                .iter()
                .find(|f| f.name == name && f.receiver.as_deref() == receiver)
                .map(|f| (index, f))
        })
    }

    fn suite_method(&self, file: usize, test: &Function, name: &str) -> Option<(usize, &Function)> {
        if !name.starts_with("Test") {
            return None;
        }
        let body = &self.files[file].lines[test.lines.clone()];
        let mut methods = self.files.iter().enumerate().flat_map(|(index, file)| {
            file.functions
                .iter()
                .filter(|f| f.name == name)
                .filter_map(move |f| Some((index, f, f.receiver.as_deref()?)))
        });
        let first = methods.next()?;
        let rest: Vec<_> = methods.collect();
        if rest.is_empty() {
            return Some((first.0, first.1));
        }
        // Several suites share the method name: pick the suite the test runs.
        let mut used = std::iter::once(first)
            .chain(rest)
            .filter(|(_, _, suite)| body.iter().any(|line| mentions(line, suite)));
        let method = used.next()?;
        used.next().is_none().then_some((method.0, method.1))
    }

    /// Lines in the scope, else outside any function (package-level tables),
    /// with a string literal whose runtime form is `segment`.
    fn named_lines(&self, file: usize, scope: &Function, segment: &str) -> Vec<SourceLocation> {
        let scope_file = &self.files[file];
        let stripped = ORDINAL.replace(segment, "");
        for wanted in [segment, stripped.as_ref()] {
            if wanted.is_empty() {
                continue;
            }
            let in_scope = scope.lines.clone().map(|line| (scope_file, line));
            let found = matching(in_scope, wanted);
            if !found.is_empty() {
                return found;
            }
            let package_level = self.files.iter().flat_map(|file| {
                (0..file.lines.len())
                    .filter(|&line| !file.functions.iter().any(|f| f.lines.contains(&line)))
                    .map(move |line| (file, line))
            });
            let found = matching(package_level, wanted);
            if !found.is_empty() {
                return found;
            }
        }
        Vec::new()
    }
}

/// Lines naming `wanted`, restricted to name positions when there are any.
fn matching<'a>(
    lines: impl Iterator<Item = (&'a File, usize)>,
    wanted: &str,
) -> Vec<SourceLocation> {
    let mut found: [Vec<SourceLocation>; 2] = Default::default();
    for (file, line) in lines {
        if let Some(rank) = literals(&file.lines[line])
            .filter(|literal| rewrite(&literal.value) == wanted)
            .map(|literal| literal.rank)
            .min()
        {
            found[rank].push(file.location(line));
        }
    }
    let [named, other] = found;
    if named.is_empty() {
        other
    } else {
        named
    }
}

impl File {
    fn parse(path: PathBuf, source: &str) -> Self {
        let lines: Vec<String> = source.lines().map(str::to_owned).collect();
        let mut functions = Vec::new();
        let mut line = 0;
        while line < lines.len() {
            let Some(captures) = FUNCTION.captures(&lines[line]) else {
                line += 1;
                continue;
            };
            let opens = lines[line].matches('{').count();
            // One-line functions close on their declaration line. Otherwise
            // gofmt puts the closing brace, alone, in the first column.
            let end = if opens > 0 && opens == lines[line].matches('}').count() {
                line + 1
            } else {
                (line + 1..lines.len())
                    .find(|&l| lines[l].starts_with('}'))
                    .map_or(lines.len(), |l| l + 1)
            };
            functions.push(Function {
                name: captures[2].to_owned(),
                receiver: captures.get(1).map(|r| r.as_str().to_owned()),
                lines: line..end,
            });
            line = end;
        }
        Self {
            path,
            lines,
            functions,
        }
    }

    fn location(&self, line: usize) -> SourceLocation {
        SourceLocation {
            path: self.path.clone(),
            line,
            message: self.lines[line].trim().to_owned(),
        }
    }
}

fn mentions(line: &str, ident: &str) -> bool {
    line.match_indices(ident).any(|(start, _)| {
        let word = |c: char| c.is_alphanumeric() || c == '_';
        !line[..start].ends_with(word) && !line[start + ident.len()..].starts_with(word)
    })
}

struct Literal {
    value: String,
    /// 0 when written where test names usually are: `t.Run("…"`, a map key,
    /// the first value of a row, or a name-like field. 1 anywhere else.
    rank: usize,
}

/// String literals on one line of Go source, outside comments. Raw strings
/// spanning lines are skipped; their remaining lines are scanned as code.
fn literals(line: &str) -> impl Iterator<Item = Literal> + '_ {
    let mut chars = line.char_indices().peekable();
    std::iter::from_fn(move || loop {
        let (start, c) = chars.next()?;
        let value = match c {
            '/' if chars.peek().is_some_and(|&(_, c)| c == '/') => return None,
            '\'' => {
                // Skip rune literals so '"' does not open a string.
                while let Some((_, c)) = chars.next() {
                    match c {
                        '\\' => {
                            chars.next();
                        }
                        '\'' => break,
                        _ => {}
                    }
                }
                continue;
            }
            '`' => {
                let mut value = String::new();
                loop {
                    match chars.next() {
                        Some((_, '`')) => break Some(value),
                        Some((_, c)) => value.push(c),
                        None => break None,
                    }
                }
            }
            '"' => {
                // None once an escape cannot be decoded; keep scanning to the
                // closing quote so the rest of the line stays in sync.
                let mut value = Some(String::new());
                loop {
                    match chars.next() {
                        Some((_, '"')) => break value,
                        None => break None,
                        Some((_, '\\')) => {
                            let decoded = chars.next().and_then(|(_, c)| unescape(c, &mut chars));
                            value = value.zip(decoded).map(|(mut value, c)| {
                                value.push(c);
                                value
                            });
                        }
                        Some((_, c)) => {
                            if let Some(value) = value.as_mut() {
                                value.push(c)
                            }
                        }
                    }
                }
            }
            _ => continue,
        };
        let Some(value) = value else { continue };
        let end = chars.peek().map_or(line.len(), |&(i, _)| i);
        let before = line[..start].trim_end();
        let rank = if before.ends_with("Run(")
            || before.ends_with('{')
            || NAME_KEY.is_match(before)
            || line[end..].trim_start().starts_with(':')
        {
            0
        } else {
            1
        };
        return Some(Literal { value, rank });
    })
}

fn unescape(
    escaped: char,
    chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
) -> Option<char> {
    let mut hex = |digits: usize| {
        let code: String = (0..digits)
            .filter_map(|_| chars.next().map(|(_, c)| c))
            .collect();
        u32::from_str_radix(&code, 16).ok().and_then(char::from_u32)
    };
    Some(match escaped {
        'a' => '\x07',
        'b' => '\x08',
        'f' => '\x0c',
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        'v' => '\x0b',
        '\\' | '"' | '\'' => escaped,
        'x' => return hex(2),
        'u' => return hex(4),
        'U' => return hex(8),
        _ => return None,
    })
}

/// Go's subtest name rewriting (testing/match.go): spaces become `_` and
/// unprintable characters their escape sequences.
fn rewrite(name: &str) -> String {
    let mut result = String::with_capacity(name.len());
    for c in name.chars() {
        let space = matches!(
            c as u32,
            0x09..=0x0d
                | 0x20
                | 0x85
                | 0xa0
                | 0x1680
                | 0x2000..=0x200a
                | 0x2028
                | 0x2029
                | 0x202f
                | 0x205f
                | 0x3000
        );
        if space {
            result.push('_');
        } else if c.is_control() {
            match c {
                '\x07' => result.push_str("\\a"),
                '\x08' => result.push_str("\\b"),
                c if (c as u32) < 0x80 => result.push_str(&format!("\\x{:02x}", c as u32)),
                c => result.push_str(&format!("\\u{:04x}", c as u32)),
            }
        } else {
            result.push(c);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = r#"package sample

import (
	"fmt"
	"testing"
)

var shared = []struct{ name string }{
	{name: "from package"},
}

func TestTable(t *testing.T) {
	tests := []struct {
		name string
		in   int
		want int
	}{
		{name: "case one", in: 1, want: 2},
		{name: "second", in: 2, want: 2},
		{name: "second", in: 3, want: 4},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			t.Errorf("got %d, want %d", tt.in, tt.want) // "case one"
		})
	}
}

func TestMap(t *testing.T) {
	for name, want := range map[string]int{
		"alpha": 1,
		"beta\tgamma": 2,
	} {
		t.Run(name, func(t *testing.T) { _ = want })
	}
}

func TestNested(t *testing.T) {
	t.Run("outer", func(t *testing.T) {
		t.Run(`inner one`, func(t *testing.T) { t.Log("inner one") })
	})
	t.Run(fmt.Sprintf("gen-%d", 1), func(t *testing.T) {})
	for _, tt := range shared {
		t.Run(tt.name, func(t *testing.T) {})
	}
}
func TestOneLine(t *testing.T) { t.Run("quick", func(t *testing.T) {}) }

func TestAfterOneLine(t *testing.T) {
	check(t, '"', "ok")
	check(t, '"', "ok")
}
"#;

    const SUITE: &str = r#"package sample

import (
	"testing"

	"github.com/stretchr/testify/suite"
)

type StoreSuite struct{ suite.Suite }
type OtherSuite struct{ suite.Suite }

func TestStore(t *testing.T) { suite.Run(t, new(StoreSuite)) }

func (s *StoreSuite) TestCreate() {
	s.Run("empty key", func() {})
}

func (s *StoreSuite) TestUnique() {}

func (o OtherSuite) TestCreate() {}
"#;

    fn package() -> (tempfile::TempDir, Package) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("table_test.go"), TABLE).unwrap();
        std::fs::write(dir.path().join("suite_test.go"), SUITE).unwrap();
        std::fs::write(dir.path().join("ignored.go"), "func TestIgnored() {}\n").unwrap();
        let package = Package::read(dir.path());
        (dir, package)
    }

    /// (file, 1-based lines, shown) for a runtime test name.
    fn resolve(package: &Package, name: &str) -> Option<(String, Vec<usize>, String)> {
        let resolution = package.resolve(name)?;
        let file = resolution.locations[0].path.file_name().unwrap();
        Some((
            file.to_string_lossy().into_owned(),
            resolution.locations.iter().map(|l| l.line + 1).collect(),
            resolution.shown,
        ))
    }

    #[test]
    fn test_functions_and_table_rows() {
        let (_dir, package) = package();
        let table = |lines: &[usize], shown: &str| {
            Some(("table_test.go".to_owned(), lines.to_vec(), shown.to_owned()))
        };
        assert_eq!(resolve(&package, "TestTable"), table(&[12], "TestTable"));
        // A row literal ranks above the same text in an argument or comment.
        assert_eq!(
            resolve(&package, "TestTable/case_one"),
            table(&[18], "TestTable/case_one")
        );
        // Repeated names: Go numbers later runs, which are later rows.
        assert_eq!(
            resolve(&package, "TestTable/second"),
            table(&[19, 20], "TestTable/second")
        );
        assert_eq!(
            resolve(&package, "TestTable/second#01"),
            table(&[20, 19], "TestTable/second#01")
        );
        assert_eq!(
            resolve(&package, "TestMap/beta_gamma"),
            table(&[32], "TestMap/beta_gamma")
        );
        // The deepest literal wins, including raw strings.
        assert_eq!(
            resolve(&package, "TestNested/outer/inner_one"),
            table(&[40], "TestNested/outer/inner_one")
        );
        assert_eq!(
            resolve(&package, "TestNested/outer"),
            table(&[39], "TestNested/outer")
        );
        // Names computed at runtime fall back to the test function.
        assert_eq!(
            resolve(&package, "TestNested/gen-1"),
            table(&[38], "TestNested")
        );
        // Tables declared outside the function.
        assert_eq!(
            resolve(&package, "TestNested/from_package"),
            table(&[9], "TestNested/from_package")
        );
        // A one-line function does not swallow the next declaration.
        assert_eq!(
            resolve(&package, "TestOneLine/quick"),
            table(&[47], "TestOneLine/quick")
        );
        // Rune literals do not open strings; plain arguments still match.
        assert_eq!(
            resolve(&package, "TestAfterOneLine/ok"),
            table(&[50, 51], "TestAfterOneLine/ok")
        );
        assert!(resolve(&package, "TestIgnored").is_none());
        assert!(resolve(&package, "TestMissing/case").is_none());
    }

    #[test]
    fn testify_suite_methods_and_their_subtests() {
        let (_dir, package) = package();
        let suite = |lines: &[usize], shown: &str| {
            Some(("suite_test.go".to_owned(), lines.to_vec(), shown.to_owned()))
        };
        assert_eq!(resolve(&package, "TestStore"), suite(&[12], "TestStore"));
        assert_eq!(
            resolve(&package, "TestStore/TestUnique"),
            suite(&[18], "TestStore/TestUnique")
        );
        // Two suites define TestCreate; the test runs StoreSuite.
        assert_eq!(
            resolve(&package, "TestStore/TestCreate"),
            suite(&[14], "TestStore/TestCreate")
        );
        assert_eq!(
            resolve(&package, "TestStore/TestCreate/empty_key"),
            suite(&[15], "TestStore/TestCreate/empty_key")
        );
        assert_eq!(
            resolve(&package, "TestStore/TestMissing"),
            suite(&[12], "TestStore")
        );
    }

    #[test]
    fn literals_decode_and_rewrite_like_go() {
        let values = |line| {
            literals(line)
                .map(|l| (rewrite(&l.value), l.rank))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            values(r#"t.Run("a b\t\"c\"", f) // "comment""#),
            [(r#"a_b_"c""#.to_owned(), 0)]
        );
        assert_eq!(
            values(r#"{"row", "\x41å", '"', "\q", `raw`},"#),
            [
                ("row".to_owned(), 0),
                ("Aå".to_owned(), 1),
                ("raw".to_owned(), 1)
            ]
        );
        assert_eq!(
            values(r#"Description: "d", want: "w", "key": 1"#),
            [
                ("d".to_owned(), 0),
                ("w".to_owned(), 1),
                ("key".to_owned(), 0)
            ]
        );
        assert_eq!(rewrite("bell\x07\x01é\u{2003}"), "bell\\a\\x01é_");
        assert!(mentions("suite.Run(t, new(StoreSuite))", "StoreSuite"));
        assert!(!mentions("new(StoreSuiteV2)", "StoreSuite"));
    }
}
