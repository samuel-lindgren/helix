//! Local Git history for replies: which commits addressed a discussion, and
//! whether a referenced commit is already part of the pull request on GitHub.
//! Only fixed argument lists are run; remote paths are validated first and are
//! passed after `--`, object ids are validated hex.
use super::github::{self, git, valid_oid};
use anyhow::{anyhow, Context as _};
use helix_view::review::{safe_text, track, Context, Hunk, Thread};
use std::collections::HashSet;

/// Why a commit was suggested for a discussion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Touch {
    /// Changed the discussed lines (followed through later edits) or inserted
    /// code directly next to them.
    Lines,
    /// Changed the discussed file after the discussion was written.
    File,
    /// Neither; listed for completeness.
    None,
}

#[derive(Clone, Debug)]
pub(super) struct Commit {
    pub sha: String,
    pub subject: String,
    pub date: String,
    pub touch: Touch,
    /// Contained in the PR head on GitHub; `None` when unknown.
    pub pushed: Option<bool>,
}

impl Commit {
    pub fn short(&self) -> &str {
        &self.sha[..self.sha.len().min(10)]
    }
    pub fn describe(&self) -> String {
        format!("{} {}", self.short(), self.subject)
    }
}

fn parse_range(text: &str) -> Option<(usize, usize)> {
    match text.split_once(',') {
        Some((start, len)) => Some((start.parse().ok()?, len.parse().ok()?)),
        None => Some((text.parse().ok()?, 1)),
    }
}

/// `@@ -a,b +c,d @@ ...`
fn parse_hunk(line: &str) -> Option<Hunk> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let (new, _) = rest.split_once(" @@")?;
    let (old_start, old_len) = parse_range(old)?;
    let (new_start, new_len) = parse_range(new)?;
    Some(Hunk {
        old_start,
        old_len,
        new_start,
        new_len,
    })
}

/// Output of `git log --format=%x01%H -p -U0`, oldest first.
fn parse_log(out: &str) -> Vec<(String, Vec<Hunk>)> {
    let mut commits: Vec<(String, Vec<Hunk>)> = Vec::new();
    for line in out.lines() {
        if let Some(sha) = line.strip_prefix('\x01') {
            commits.push((sha.trim().to_owned(), Vec::new()));
        } else if let (Some(hunk), Some((_, hunks))) = (parse_hunk(line), commits.last_mut()) {
            hunks.push(hunk);
        }
    }
    commits
}

async fn has_commit(context: &Context, oid: &str) -> bool {
    valid_oid(oid)
        && git(context, &["cat-file", "-e", &format!("{oid}^{{commit}}")])
            .await
            .is_ok()
}

async fn is_ancestor(context: &Context, ancestor: &str, descendant: &str) -> bool {
    git(
        context,
        &["merge-base", "--is-ancestor", ancestor, descendant],
    )
    .await
    .is_ok()
}

/// Commits on HEAD after the discussion's commit that touched its lines and its
/// file, each newest first. Empty when the discussion commit is not in local
/// history (for example after a rebase) or its path is unusable.
pub(super) async fn touching(
    context: &Context,
    thread: &Thread,
) -> anyhow::Result<(Vec<String>, Vec<String>)> {
    let Some(base) = thread.commit.as_deref() else {
        return Ok(Default::default());
    };
    if !github::valid_path(&thread.path)
        || !has_commit(context, base).await
        || !is_ancestor(context, base, "HEAD").await
    {
        return Ok(Default::default());
    }
    let out = git(
        context,
        &[
            "-c",
            "core.quotePath=false",
            "log",
            "--reverse",
            "--first-parent",
            "--diff-merges=first-parent",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "-p",
            "-U0",
            "--format=%x01%H",
            &format!("{base}..HEAD"),
            "--",
            &thread.path,
        ],
    )
    .await?;
    let mut range = thread.original_lines.clone();
    let mut lines = Vec::new();
    let mut file = Vec::new();
    for (sha, hunks) in parse_log(&out) {
        if !valid_oid(&sha) {
            continue;
        }
        if let Some(current) = range.take() {
            let (touched, next) = track(current, &hunks);
            if touched {
                lines.push(sha.clone());
            }
            range = Some(next);
        }
        file.push(sha);
    }
    lines.reverse();
    file.reverse();
    Ok((lines, file))
}

async fn describe(context: &Context, rev: &str) -> anyhow::Result<Commit> {
    let out = git(
        context,
        &[
            "log",
            "-1",
            "--no-color",
            "--format=%H%x1f%s%x1f%as",
            "--end-of-options",
            rev,
        ],
    )
    .await?;
    parse_commit(&out).ok_or_else(|| anyhow!("Cannot read commit {}", safe_text(rev)))
}

fn parse_commit(line: &str) -> Option<Commit> {
    let mut fields = line.trim_end().split('\x1f');
    let sha = fields.next()?.trim();
    if !valid_oid(sha) {
        return None;
    }
    Some(Commit {
        sha: sha.to_owned(),
        subject: safe_text(fields.next().unwrap_or("")).replace('\n', " "),
        date: safe_text(fields.next().unwrap_or("")),
        touch: Touch::None,
        pushed: None,
    })
}

/// The commit most likely to address a discussion: the newest commit touching
/// its lines, else the newest touching its file since it was written, else HEAD.
pub(super) async fn suggest(context: &Context, thread: Option<&Thread>) -> anyhow::Result<Commit> {
    let (lines, file) = match thread {
        Some(thread) => touching(context, thread).await.unwrap_or_default(),
        None => Default::default(),
    };
    let (rev, touch) = if let Some(sha) = lines.first() {
        (sha.as_str(), Touch::Lines)
    } else if let Some(sha) = file.first() {
        (sha.as_str(), Touch::File)
    } else {
        ("HEAD", Touch::None)
    };
    let mut commit = describe(context, rev).await?;
    commit.touch = touch;
    Ok(commit)
}

/// Resolve a user-supplied revision to a commit.
pub(super) async fn resolve(context: &Context, rev: &str) -> anyhow::Result<Commit> {
    let sha = git(
        context,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &format!("{rev}^{{commit}}"),
        ],
    )
    .await
    .map_err(|_| anyhow!("Unknown commit {}", safe_text(rev)))?;
    describe(context, &sha).await
}

/// Commits reachable from HEAD but not from `head` (the PR head on GitHub).
/// `None` when that head is not available locally, so nothing can be verified.
async fn unpushed(context: &Context, head: &str) -> Option<HashSet<String>> {
    if !has_commit(context, head).await {
        return None;
    }
    let out = git(
        context,
        &["rev-list", "--max-count=1000", "HEAD", "--not", head],
    )
    .await
    .ok()?;
    Some(out.lines().map(str::to_owned).collect())
}

/// Recent branch commits for the insert picker: commits touching the
/// discussion first, then the rest, newest first within each group.
pub(super) async fn list(
    context: &Context,
    thread: Option<&Thread>,
    pull: Option<&github::PullHead>,
) -> anyhow::Result<Vec<Commit>> {
    let mut args = vec![
        "log",
        "--no-color",
        "--max-count=50",
        "--format=%H%x1f%s%x1f%as",
        "HEAD",
    ];
    let base = match pull.and_then(|p| p.base.as_deref()) {
        Some(base) if has_commit(context, base).await => Some(format!("^{base}")),
        _ => None,
    };
    if let Some(base) = &base {
        args.push(base);
    }
    args.push("--");
    let out = git(context, &args).await.context("Cannot list commits")?;
    let mut commits: Vec<_> = out.lines().filter_map(parse_commit).collect();
    let (lines, file) = match thread {
        Some(thread) => touching(context, thread).await.unwrap_or_default(),
        None => Default::default(),
    };
    let unpushed = match pull {
        Some(pull) => unpushed(context, &pull.oid).await,
        None => None,
    };
    for commit in &mut commits {
        commit.touch = if lines.contains(&commit.sha) {
            Touch::Lines
        } else if file.contains(&commit.sha) {
            Touch::File
        } else {
            Touch::None
        };
        commit.pushed = unpushed.as_ref().map(|set| !set.contains(&commit.sha));
    }
    // Stable: history order is kept within each group.
    commits.sort_by_key(|c| match c.touch {
        Touch::Lines => 0,
        Touch::File => 1,
        Touch::None => 2,
    });
    Ok(commits)
}

/// Hexadecimal words that may be commit ids (7 to 40 digits), in order, unique.
fn hash_candidates(body: &str) -> Vec<&str> {
    let mut seen = HashSet::new();
    body.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| {
            (7..=40).contains(&word.len()) && word.bytes().all(|b| b.is_ascii_hexdigit())
        })
        .filter(|word| seen.insert(*word))
        .take(20)
        .collect()
}

/// Whether `body` contains a word that may be a commit id.
pub(super) fn mentions_commit(body: &str) -> bool {
    !hash_candidates(body).is_empty()
}

/// Commits referenced in `body` that exist locally but are not contained in the
/// PR head on GitHub; GitHub cannot link those. Words that are not local commits
/// are ignored. `Err` when the PR head is not available locally for checking.
pub(super) async fn unpushed_references(
    context: &Context,
    body: &str,
    pull: &github::PullHead,
) -> anyhow::Result<Vec<String>> {
    let mut referenced = Vec::new();
    for word in hash_candidates(body) {
        let Ok(sha) = git(
            context,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &format!("{word}^{{commit}}"),
            ],
        )
        .await
        else {
            continue;
        };
        if valid_oid(&sha) {
            referenced.push((word, sha));
        }
    }
    if referenced.is_empty() {
        return Ok(Vec::new());
    }
    anyhow::ensure!(
        has_commit(context, &pull.oid).await,
        "PR head {} ({}) is not in the local repository, so referenced commits cannot be verified; run git fetch",
        &pull.oid[..pull.oid.len().min(10)],
        pull.name
    );
    let mut missing = Vec::new();
    for (word, sha) in referenced {
        if !is_ancestor(context, &sha, &pull.oid).await {
            missing.push(word[..word.len().min(10)].to_owned());
        }
    }
    Ok(missing)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunk(old_start: usize, old_len: usize, new_start: usize, new_len: usize) -> Hunk {
        Hunk {
            old_start,
            old_len,
            new_start,
            new_len,
        }
    }

    #[test]
    fn hunk_headers_and_log_parse() {
        assert_eq!(parse_hunk("@@ -3 +3,2 @@ fn x()"), Some(hunk(3, 1, 3, 2)));
        assert_eq!(parse_hunk("@@ -10,0 +11,4 @@"), Some(hunk(10, 0, 11, 4)));
        assert_eq!(parse_hunk("+@@ -1 +1 @@"), None);
        let log = "\x01aaa\ndiff --git a/x b/x\n@@ -1 +1 @@\n-a\n+b\n+@@ -9 +9 @@\n\x01bbb\n";
        assert_eq!(
            parse_log(log),
            vec![
                ("aaa".into(), vec![hunk(1, 1, 1, 1)]),
                ("bbb".into(), vec![])
            ]
        );
    }

    #[test]
    fn hash_words_are_bounded_and_unique() {
        assert_eq!(
            hash_candidates("Fixed in 0123abcd, see 0123abcd and deadbee! not cafe or g123456x"),
            vec!["0123abcd", "deadbee"]
        );
        assert!(hash_candidates(&"a".repeat(41)).is_empty());
    }

    #[cfg(unix)]
    fn sh(dir: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    /// A temporary repository: a reviewed commit, an unrelated edit that shifts
    /// the discussed lines, the fix, and a later commit touching another file.
    #[cfg(unix)]
    #[tokio::test]
    async fn suggestion_follows_discussed_lines_through_history() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sh(root, &["init", "--quiet", "--initial-branch=topic"]);
        let file: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        std::fs::write(root.join("a.rs"), &file).unwrap();
        std::fs::write(root.join("b.rs"), "b\n").unwrap();
        sh(root, &["add", "."]);
        sh(root, &["commit", "--quiet", "-m", "reviewed"]);
        let reviewed = sh(root, &["rev-parse", "HEAD"]);
        std::fs::write(root.join("a.rs"), format!("header\n{file}")).unwrap();
        sh(root, &["commit", "--quiet", "-am", "shift"]);
        let shift = sh(root, &["rev-parse", "HEAD"]);
        // Discussed lines 10-11 are now 11-12; change line 11 ("line 10").
        std::fs::write(
            root.join("a.rs"),
            format!("header\n{file}").replace("line 10\n", "line 10 fixed\n"),
        )
        .unwrap();
        sh(root, &["commit", "--quiet", "-am", "fix: handle line 10"]);
        let fix = sh(root, &["rev-parse", "HEAD"]);
        std::fs::write(root.join("b.rs"), "b2\n").unwrap();
        sh(root, &["commit", "--quiet", "-am", "unrelated"]);
        let head = sh(root, &["rev-parse", "HEAD"]);

        let context = Context {
            root: root.into(),
            git_dir: root.join(".git"),
            branch: "topic".into(),
            head: head.clone(),
            config: vec![],
        };
        let thread = Thread {
            id: "t".into(),
            path: "a.rs".into(),
            commit: Some(reviewed.clone()),
            original_lines: Some(10..12),
            ..Thread::default()
        };
        let (lines, file_commits) = touching(&context, &thread).await.unwrap();
        assert_eq!(lines, vec![fix.clone()]);
        assert_eq!(file_commits, vec![fix.clone(), shift.clone()]);
        let suggestion = suggest(&context, Some(&thread)).await.unwrap();
        assert_eq!(suggestion.sha, fix);
        assert_eq!(suggestion.touch, Touch::Lines);
        assert_eq!(suggestion.subject, "fix: handle line 10");

        // Lines never touched after the review: newest commit on the file.
        let untouched = Thread {
            original_lines: Some(18..19),
            ..thread.clone()
        };
        let suggestion = suggest(&context, Some(&untouched)).await.unwrap();
        assert_eq!(
            (suggestion.sha.as_str(), suggestion.touch),
            (fix.as_str(), Touch::File)
        );
        // Unknown review commit (e.g. rebased away): HEAD.
        let rebased = Thread {
            commit: Some("0".repeat(40)),
            ..thread.clone()
        };
        let suggestion = suggest(&context, Some(&rebased)).await.unwrap();
        assert_eq!(
            (suggestion.sha.as_str(), suggestion.touch),
            (head.as_str(), Touch::None)
        );

        // Picker order and pushed markers relative to a PR head at `fix`.
        let pull = github::PullHead {
            oid: fix.clone(),
            name: "me/fork:topic".into(),
            base: Some(reviewed.clone()),
        };
        let listed = list(&context, Some(&thread), Some(&pull)).await.unwrap();
        let order: Vec<_> = listed
            .iter()
            .map(|c| (c.sha.as_str(), c.touch, c.pushed))
            .collect();
        assert_eq!(
            order,
            vec![
                (fix.as_str(), Touch::Lines, Some(true)),
                (shift.as_str(), Touch::File, Some(true)),
                (head.as_str(), Touch::None, Some(false)),
            ]
        );

        // Unpushed references are detected by any unique prefix; other hex words
        // and pushed commits pass.
        let body = format!("Fixed in {} and {}; ref deadbeef1", &fix[..8], &head[..12]);
        assert_eq!(
            unpushed_references(&context, &body, &pull).await.unwrap(),
            vec![head[..10].to_owned()]
        );
        let unknown_head = github::PullHead {
            oid: "1".repeat(40),
            ..pull.clone()
        };
        assert!(unpushed_references(&context, &body, &unknown_head)
            .await
            .unwrap_err()
            .to_string()
            .contains("git fetch"));
        assert!(unpushed_references(&context, "no hashes", &unknown_head)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(resolve(&context, "HEAD~1").await.unwrap().sha, fix);
        assert!(resolve(&context, "--output=/tmp/x").await.is_err());
    }
}
