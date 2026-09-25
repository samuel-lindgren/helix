//! `git status --porcelain=v2 -z` and per-file diff sections. No process I/O.
use anyhow::{anyhow, bail, ensure};
use std::collections::HashMap;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Branch {
    /// `None` for a detached HEAD.
    pub name: Option<String>,
    /// `None` before the first commit.
    pub oid: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
}

/// Which side of a file's changes a row describes. A partly staged file has a
/// `Staged` and an `Unstaged` row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Part {
    /// A buffer with unsaved edits; Git cannot see them.
    Unsaved,
    Conflict,
    Staged,
    Unstaged,
    Untracked,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Change {
    /// Repository-relative, `/`-separated.
    pub path: String,
    /// Source of a staged rename or copy.
    pub orig: Option<String>,
    pub part: Part,
    /// Git's status letter for this part (`M`, `A`, `D`, `R`, `C`, `T`, `U`, `?`).
    pub code: char,
    /// The file also has changes on the other side of the index.
    pub partial: bool,
}

impl Change {
    /// `staged modified`, `unstaged deleted (partly staged)`, `untracked`, …
    pub fn describe(&self) -> String {
        let kind = match self.code {
            'M' => "modified",
            'A' => "new file",
            'D' => "deleted",
            'R' => "renamed",
            'C' => "copied",
            'T' => "type changed",
            _ => "changed",
        };
        let text = match self.part {
            Part::Unsaved => "unsaved buffer".to_owned(),
            Part::Conflict => "conflict".to_owned(),
            Part::Untracked => "untracked".to_owned(),
            Part::Staged => format!("staged {kind}"),
            Part::Unstaged => format!("unstaged {kind}"),
        };
        if self.partial {
            format!("{text} (partly staged)")
        } else {
            text
        }
    }

    /// `path`, or `orig -> path` for renames and copies.
    pub fn display_path(&self) -> String {
        match &self.orig {
            Some(orig) => format!("{orig} -> {}", self.path),
            None => self.path.clone(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Status {
    pub branch: Branch,
    pub changes: Vec<Change>,
}

fn text(bytes: &[u8]) -> anyhow::Result<&str> {
    std::str::from_utf8(bytes).map_err(|_| anyhow!("Git reported a path that is not UTF-8"))
}

/// Parse `git status --porcelain=v2 -z --branch` output. Changes are sorted:
/// conflicts, staged, unstaged, untracked; by path within each group.
pub(crate) fn parse(out: &[u8]) -> anyhow::Result<Status> {
    let mut status = Status::default();
    let mut records = out.split(|&b| b == 0).filter(|r| !r.is_empty());
    while let Some(record) = records.next() {
        let record = text(record)?;
        let (kind, rest) = record.split_once(' ').unwrap_or((record, ""));
        match kind {
            "#" => {
                let (key, value) = rest.split_once(' ').unwrap_or((rest, ""));
                match key {
                    "branch.oid" if value != "(initial)" => status.branch.oid = Some(value.into()),
                    "branch.head" if value != "(detached)" => {
                        status.branch.name = Some(value.into())
                    }
                    "branch.upstream" => status.branch.upstream = Some(value.into()),
                    "branch.ab" => {
                        let mut counts = value
                            .split(' ')
                            .map(|n| n.trim_start_matches(['+', '-']).parse().unwrap_or(0));
                        status.branch.ahead = counts.next().unwrap_or(0);
                        status.branch.behind = counts.next().unwrap_or(0);
                    }
                    _ => {}
                }
            }
            "1" | "2" => {
                let fields: Vec<_> = rest.splitn(if kind == "1" { 8 } else { 9 }, ' ').collect();
                ensure!(
                    fields.len() == if kind == "1" { 8 } else { 9 },
                    "Unexpected git status record"
                );
                let xy: Vec<char> = fields[0].chars().collect();
                ensure!(xy.len() == 2, "Unexpected git status record");
                let path = fields[fields.len() - 1].to_owned();
                let orig = if kind == "2" {
                    let orig = records
                        .next()
                        .ok_or_else(|| anyhow!("Unexpected git status record"))?;
                    Some(text(orig)?.to_owned())
                } else {
                    None
                };
                let (index, worktree) = (xy[0], xy[1]);
                if index != '.' {
                    status.changes.push(Change {
                        path: path.clone(),
                        orig,
                        part: Part::Staged,
                        code: index,
                        partial: worktree != '.',
                    });
                }
                if worktree != '.' {
                    status.changes.push(Change {
                        path,
                        orig: None,
                        part: Part::Unstaged,
                        code: worktree,
                        partial: index != '.',
                    });
                }
            }
            "u" => {
                let fields: Vec<_> = rest.splitn(10, ' ').collect();
                ensure!(fields.len() == 10, "Unexpected git status record");
                status.changes.push(Change {
                    path: fields[9].to_owned(),
                    orig: None,
                    part: Part::Conflict,
                    code: 'U',
                    partial: false,
                });
            }
            "?" => status.changes.push(Change {
                path: rest.to_owned(),
                orig: None,
                part: Part::Untracked,
                code: '?',
                partial: false,
            }),
            "!" => {}
            _ => bail!("Unexpected git status record"),
        }
    }
    status
        .changes
        .sort_by(|a, b| (a.part, &a.path).cmp(&(b.part, &b.path)));
    Ok(status)
}

/// Split `git diff --no-renames --src-prefix=a/ --dst-prefix=b/` output into
/// sections by path. Without renames both header paths are equal, which makes
/// paths with spaces unambiguous; quoted (unusual) paths get no section.
pub(crate) fn split_diff(patch: &str) -> HashMap<String, String> {
    let mut sections: HashMap<String, String> = HashMap::new();
    let mut current: Option<String> = None;
    for line in patch.split_inclusive('\n') {
        if let Some(header) = line.strip_prefix("diff --git ") {
            let header = header.trim_end_matches(['\n', '\r']);
            current = header
                .len()
                .checked_sub(5)
                .filter(|n| n % 2 == 0)
                .map(|n| n / 2)
                .and_then(|len| {
                    let a = header.get(2..2 + len)?;
                    (header.starts_with("a/") && header.get(2 + len..)? == format!(" b/{a}"))
                        .then(|| a.to_owned())
                });
        }
        if let Some(path) = &current {
            sections.entry(path.clone()).or_default().push_str(line);
        }
    }
    sections
}

/// `git diff --name-status -z` output: `(status letter, path)` pairs.
pub(crate) fn parse_name_status(out: &[u8]) -> anyhow::Result<Vec<(char, String)>> {
    let mut fields = out.split(|&b| b == 0).filter(|r| !r.is_empty());
    let mut entries = Vec::new();
    while let Some(status) = fields.next() {
        let code = text(status)?
            .chars()
            .next()
            .ok_or_else(|| anyhow!("Unexpected git diff record"))?;
        let path = fields
            .next()
            .ok_or_else(|| anyhow!("Unexpected git diff record"))?;
        entries.push((code, text(path)?.to_owned()));
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_v2_records() {
        let out = [
            "# branch.oid 0123456789abcdef0123456789abcdef01234567",
            "# branch.head topic",
            "# branch.upstream origin/topic",
            "# branch.ab +2 -1",
            "1 MM N... 100644 100644 100644 aaa bbb src/partly staged.rs",
            "1 .D N... 100644 100644 000000 aaa aaa gone.rs",
            "1 A. N... 000000 100644 100644 000 ccc new.rs",
            "2 R. N... 100644 100644 100644 aaa aaa R100 moved.rs",
            "old name.rs",
            "u UU N... 100644 100644 100644 100644 a b c conflict.rs",
            "? untracked dir/",
            "! ignored.rs",
        ]
        .join("\0");
        let status = parse(out.as_bytes()).unwrap();
        assert_eq!(
            status.branch,
            Branch {
                name: Some("topic".into()),
                oid: Some("0123456789abcdef0123456789abcdef01234567".into()),
                upstream: Some("origin/topic".into()),
                ahead: 2,
                behind: 1,
            }
        );
        let rows: Vec<_> = status
            .changes
            .iter()
            .map(|c| format!("{} | {}", c.describe(), c.display_path()))
            .collect();
        assert_eq!(
            rows,
            [
                "conflict | conflict.rs",
                "staged renamed | old name.rs -> moved.rs",
                "staged new file | new.rs",
                "staged modified (partly staged) | src/partly staged.rs",
                "unstaged deleted | gone.rs",
                "unstaged modified (partly staged) | src/partly staged.rs",
                "untracked | untracked dir/",
            ]
        );
        let staged = status.changes.iter().filter(|c| c.part == Part::Staged);
        assert_eq!(staged.count(), 3);

        let initial = parse(b"# branch.oid (initial)\0# branch.head (detached)\0").unwrap();
        assert_eq!(initial.branch, Branch::default());
        assert!(parse(b"1 M\0").is_err());
        assert!(parse(b"? \xff\0").is_err());
    }

    #[test]
    fn diff_sections_by_path() {
        let patch = "diff --git a/a b/a\nindex 1..2\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-x\n+y\n\
diff --git a/with space b/with space\nnew file mode 100644\n\
diff --git a/t b/t\ndeleted file mode 100644\ndiff --git a/t b/t\nnew file mode 120000\n\
diff --git \"a/q\\tx\" \"b/q\\tx\"\n+quoted\n";
        let sections = split_diff(patch);
        assert_eq!(
            sections["a"],
            "diff --git a/a b/a\nindex 1..2\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-x\n+y\n"
        );
        assert_eq!(
            sections["with space"],
            "diff --git a/with space b/with space\nnew file mode 100644\n"
        );
        // A type change is two sections for one path.
        assert_eq!(sections["t"].matches("diff --git").count(), 2);
        assert_eq!(sections.len(), 3);
        assert_eq!(
            parse_name_status(b"M\0a.rs\0A\0dir/b c.rs\0").unwrap(),
            vec![('M', "a.rs".into()), ('A', "dir/b c.rs".into())]
        );
    }
}
