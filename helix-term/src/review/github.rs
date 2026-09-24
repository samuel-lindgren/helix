//! GitHub CLI transport. All API operations are queries; authentication belongs
//! to gh. Never invoke a shell or use remote strings as options or filesystem paths.
use anyhow::{anyhow, bail, ensure, Context as _};
use helix_view::review::{safe_text, Comment, Context, Review, Thread};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    path::{Component, Path},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};

const LIMIT: u64 = 16 * 1024 * 1024;
const MAX_PAGES: usize = 100;

async fn run(
    root: &Path,
    program: &str,
    args: &[&str],
    input: Option<Vec<u8>>,
) -> anyhow::Result<String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(root)
        .kill_on_drop(true)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_PAGER", "cat")
        .env_remove("GH_REPO")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .with_context(|| format!("Cannot run {program}"))?;
    let mut stdout = child.stdout.take().unwrap().take(LIMIT + 1);
    let mut stderr = child.stderr.take().unwrap().take(LIMIT + 1);
    let mut stdin = child.stdin.take();
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        let mut out = Vec::new();
        let mut err = Vec::new();
        tokio::try_join!(
            async {
                if let (Some(mut stdin), Some(input)) = (stdin.take(), input) {
                    stdin.write_all(&input).await?;
                    stdin.shutdown().await?;
                }
                Ok::<_, std::io::Error>(())
            },
            stdout.read_to_end(&mut out),
            stderr.read_to_end(&mut err)
        )?;
        ensure!(
            out.len() as u64 <= LIMIT && err.len() as u64 <= LIMIT,
            "{program} output exceeded review size limit"
        );
        let status = child.wait().await?;
        ensure!(
            status.success(),
            "{program}: {}",
            safe_text(&String::from_utf8_lossy(&err))
                .chars()
                .take(500)
                .collect::<String>()
        );
        Ok(String::from_utf8(out)?)
    })
    .await
    .context("Review command timed out")?;
    result
}

async fn git(context: &Context, args: &[&str]) -> anyhow::Result<String> {
    Ok(run(&context.root, "git", args, None)
        .await?
        .trim()
        .to_owned())
}
struct Transport<'a> {
    context: &'a Context,
    gh: &'a str,
}

async fn api(transport: &Transport<'_>, query: &str, variables: Value) -> anyhow::Result<Value> {
    let context = transport.context;
    let input = serde_json::to_vec(&json!({"query":query,"variables":variables}))?;
    let out = run(
        &context.root,
        transport.gh,
        &["api", "--hostname", "github.com", "graphql", "--input", "-"],
        Some(input),
    )
    .await?;
    let value: Value = serde_json::from_str(&out)?;
    ensure!(
        value.get("errors").is_none(),
        "GitHub query failed: {}",
        safe_text(&value["errors"].to_string())
            .chars()
            .take(500)
            .collect::<String>()
    );
    value
        .get("data")
        .cloned()
        .ok_or_else(|| anyhow!("GitHub returned no data"))
}

pub(super) fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains(['\\', ':'])
        && !path.chars().any(char::is_control)
        && !path
            .split('/')
            .any(|s| s.is_empty() || s == "." || s == ".." || s.eq_ignore_ascii_case(".git"))
        && Path::new(path)
            .components()
            .all(|p| matches!(p, Component::Normal(_)))
}

fn repository(url: &str) -> Option<String> {
    let path = if let Some(path) = url.strip_prefix("git@github.com:") {
        path.to_owned()
    } else {
        let url = url::Url::parse(url).ok()?;
        if url.host_str()? != "github.com" {
            return None;
        }
        url.path().trim_start_matches('/').to_owned()
    };
    let path = path.trim_end_matches('/').trim_end_matches(".git");
    let parts: Vec<_> = path.split('/').collect();
    (parts.len() == 2
        && parts.iter().all(|s| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        }))
    .then(|| path.to_owned())
}
fn names(repo: &str) -> anyhow::Result<(&str, &str)> {
    repo.split_once('/')
        .ok_or_else(|| anyhow!("Invalid repository identity"))
}
fn string<'a>(value: &'a Value, key: &str) -> anyhow::Result<&'a str> {
    value[key]
        .as_str()
        .ok_or_else(|| anyhow!("Missing GitHub field {key}"))
}
fn nodes(value: &Value) -> anyhow::Result<&Vec<Value>> {
    value["nodes"]
        .as_array()
        .ok_or_else(|| anyhow!("Missing GitHub connection"))
}
fn cursor(value: &Value) -> anyhow::Result<Option<String>> {
    match value["pageInfo"]["hasNextPage"].as_bool() {
        Some(false) => Ok(None),
        Some(true) => Ok(Some(string(&value["pageInfo"], "endCursor")?.to_owned())),
        None => bail!("Missing GitHub pagination state"),
    }
}

#[derive(Clone, Debug)]
struct Pull {
    repo: String,
    number: u64,
    url: String,
}

const DISCOVER: &str = r#"query($owner:String!,$repo:String!,$branch:String!,$after:String) {
 repository(owner:$owner,name:$repo) { pullRequests(headRefName:$branch,states:OPEN,first:100,after:$after) {
 nodes { number url headRefName headRepository { nameWithOwner } } pageInfo { hasNextPage endCursor }
 } } }"#;

/// Push identity matters in triangular forks: a feature may fetch upstream/main
/// while pushing origin/feature. Never substitute its fetch-upstream branch.
fn head_target(
    repos: &HashMap<String, String>,
    branch: &str,
    tracking: Option<&str>,
    push_remote: Option<&str>,
    push_ref: Option<&str>,
) -> anyhow::Result<(String, String)> {
    if let Some(reference) = push_ref.and_then(|r| r.strip_prefix("refs/remotes/")) {
        let mut candidates: Vec<_> = repos
            .iter()
            .filter_map(|(remote, repo)| {
                reference
                    .strip_prefix(&format!("{remote}/"))
                    .map(|branch| (remote, repo, branch))
            })
            .collect();
        candidates.sort_by_key(|(remote, _, _)| std::cmp::Reverse(remote.len()));
        if let Some((_, repo, branch)) = candidates.first() {
            return Ok(((*repo).clone(), (*branch).to_owned()));
        }
    }
    let repo = if let Some(remote) = push_remote {
        repos
            .get(remote)
            .ok_or_else(|| anyhow!("Configured push remote is not a supported GitHub repository"))?
    } else {
        repos
            .get("origin")
            .or_else(|| tracking.and_then(|r| repos.get(r)))
            .or_else(|| (repos.len() == 1).then(|| repos.values().next().unwrap()))
            .ok_or_else(|| {
                anyhow!(
                    "Cannot identify GitHub head repository; use :review-select owner/repo#number"
                )
            })?
    };
    Ok((repo.clone(), branch.to_owned()))
}

async fn discover(transport: &Transport<'_>) -> anyhow::Result<Option<Pull>> {
    let context = transport.context;
    let remotes = git(context, &["remote"]).await?;
    let mut repos = HashMap::new();
    let mut bases = HashSet::new();
    for remote in remotes.lines() {
        // get-url applies Git's insteadOf/pushInsteadOf and pushurl configuration.
        let fetch_urls = git(context, &["remote", "get-url", "--all", remote]).await?;
        bases.extend(fetch_urls.lines().filter_map(repository));
        let push_urls = git(context, &["remote", "get-url", "--push", "--all", remote]).await?;
        let pushes: HashSet<_> = push_urls.lines().filter_map(repository).collect();
        ensure!(pushes.len() <= 1, "Remote {remote} has multiple GitHub push destinations; use :review-select owner/repo#number");
        if let Some(repo) = pushes.into_iter().next() {
            bases.insert(repo.clone());
            repos.insert(remote.to_owned(), repo);
        }
    }
    let remote_key = format!("branch.{}.remote", context.branch);
    let tracking = git(context, &["config", "--get", &remote_key]).await.ok();
    let push_key = format!("branch.{}.pushRemote", context.branch);
    let push_remote = match git(context, &["config", "--get", &push_key]).await.ok() {
        Some(remote) => Some(remote),
        None => git(context, &["config", "--get", "remote.pushDefault"])
            .await
            .ok(),
    };
    let push_ref = git(context, &["rev-parse", "--symbolic-full-name", "@{push}"])
        .await
        .ok();
    let (head_repo, branch) = head_target(
        &repos,
        &context.branch,
        tracking.as_deref(),
        push_remote.as_deref(),
        push_ref.as_deref(),
    )?;
    let (owner, name) = names(&head_repo)?;
    let parent = api(transport, "query($owner:String!,$repo:String!) { repository(owner:$owner,name:$repo) { parent { nameWithOwner } } }", json!({"owner":owner,"repo":name})).await?;
    if let Some(parent) = parent["repository"]["parent"]["nameWithOwner"].as_str() {
        bases.insert(parent.to_owned());
    }
    // Own fork and its parent can both have a PR for the same branch. Never pick
    // the first match. Discovery must finish in every candidate base repository.
    let head_repo = format!("{owner}/{name}");
    let mut pulls = Vec::new();
    for base in bases {
        let (owner, repo) = names(&base)?;
        let mut after: Option<String> = None;
        for page in 0..MAX_PAGES {
            let data = api(
                transport,
                DISCOVER,
                json!({"owner":owner,"repo":repo,"branch":branch,"after":after}),
            )
            .await?;
            let connection = &data["repository"]["pullRequests"];
            for pr in nodes(connection)? {
                if pr["headRepository"]["nameWithOwner"]
                    .as_str()
                    .is_some_and(|r| r.eq_ignore_ascii_case(&head_repo))
                    && pr["headRefName"] == branch
                {
                    pulls.push(Pull {
                        repo: base.clone(),
                        number: pr["number"]
                            .as_u64()
                            .ok_or_else(|| anyhow!("Missing PR number"))?,
                        url: safe_text(string(pr, "url")?),
                    });
                }
            }
            after = cursor(connection)?;
            if after.is_none() {
                break;
            }
            ensure!(
                page + 1 < MAX_PAGES,
                "Too many PR pages; cannot establish unambiguous association"
            );
        }
    }
    unique_pull(pulls)
}

fn unique_pull(mut pulls: Vec<Pull>) -> anyhow::Result<Option<Pull>> {
    ensure!(pulls.len() <= 1, "Ambiguous branch association: {}. Use :review-select owner/repo#number to choose for this context.", pulls.iter().map(|p| p.url.as_str()).collect::<Vec<_>>().join(", "));
    Ok(pulls.pop())
}

const THREADS: &str = r#"query($owner:String!,$repo:String!,$number:Int!,$after:String) {
 repository(owner:$owner,name:$repo) { pullRequest(number:$number) { headRefOid updatedAt url
 reviewThreads(first:100,after:$after) { nodes { id path line startLine diffSide startDiffSide originalLine originalStartLine isOutdated isResolved
 comments(first:100) { nodes { author { login } body diffHunk url originalCommit { oid } } pageInfo { hasNextPage endCursor } }
 } pageInfo { hasNextPage endCursor } } } } }"#;
const COMMENTS: &str = r#"query($id:ID!,$after:String) { node(id:$id) { ... on PullRequestReviewThread {
 comments(first:100,after:$after) { nodes { author { login } body diffHunk url originalCommit { oid } } pageInfo { hasNextPage endCursor } }
 } } }"#;

fn parse_thread(value: &Value, comments: Vec<Value>) -> anyhow::Result<Thread> {
    let path = string(value, "path")?;
    let line = value["line"].as_u64().and_then(|n| usize::try_from(n).ok());
    let start = value["startLine"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .or(line);
    let right = value["diffSide"] == "RIGHT"
        && (value["startDiffSide"].is_null() || value["startDiffSide"] == "RIGHT");
    let outdated = value["isOutdated"].as_bool().unwrap_or(true);
    let lines = if valid_path(path) && right && !outdated {
        start
            .zip(line)
            .filter(|(s, e)| *s > 0 && s <= e)
            .and_then(|(s, e)| e.checked_add(1).map(|end| s..end))
    } else {
        None
    };
    let first = comments
        .first()
        .ok_or_else(|| anyhow!("Empty review thread"))?;
    let status = if outdated {
        "outdated"
    } else if !right {
        "old/deleted side"
    } else if lines.is_none() {
        "location unavailable"
    } else {
        "PR head"
    };
    let location = format!(
        "{}:{}–{} ({}, {}, original {}–{}, commit {})",
        safe_text(path),
        start.map_or("?".into(), |n| n.to_string()),
        line.map_or("?".into(), |n| n.to_string()),
        status,
        safe_text(value["diffSide"].as_str().unwrap_or("?")),
        value["originalStartLine"],
        value["originalLine"],
        safe_text(first["originalCommit"]["oid"].as_str().unwrap_or("unknown"))
    );
    Ok(Thread {
        id: string(value, "id")?.to_owned(),
        path: path.to_owned(),
        lines,
        location,
        resolved: value["isResolved"].as_bool().unwrap_or(false),
        diff: safe_text(
            first["diffHunk"]
                .as_str()
                .unwrap_or("Original diff unavailable"),
        ),
        url: safe_text(first["url"].as_str().unwrap_or("")),
        comments: comments
            .iter()
            .map(|c| Comment {
                author: safe_text(c["author"]["login"].as_str().unwrap_or("[deleted]")),
                body: safe_text(c["body"].as_str().unwrap_or("")),
            })
            .collect(),
    })
}

pub(super) async fn fetch(
    context: &Context,
    selection: Option<String>,
) -> anyhow::Result<Option<Review>> {
    fetch_with(&Transport { context, gh: "gh" }, selection).await
}

async fn fetch_with(
    transport: &Transport<'_>,
    selection: Option<String>,
) -> anyhow::Result<Option<Review>> {
    let pull = if let Some(selection) = selection {
        let (repo, number) = selection
            .split_once('#')
            .ok_or_else(|| anyhow!("Expected owner/repo#number"))?;
        ensure!(
            repository(&format!("https://github.com/{repo}")) == Some(repo.to_owned()),
            "Invalid repository"
        );
        Pull {
            repo: repo.into(),
            number: number.parse()?,
            url: String::new(),
        }
    } else if let Some(pull) = discover(transport).await? {
        pull
    } else {
        return Ok(None);
    };
    let (owner, repo) = names(&pull.repo)?;
    let mut after: Option<String> = None;
    let mut threads = Vec::new();
    let mut content_bytes = 0usize;
    let mut revision = None;
    let mut updated = None;
    for page in 0..MAX_PAGES {
        let data = api(
            transport,
            THREADS,
            json!({"owner":owner,"repo":repo,"number":pull.number,"after":after}),
        )
        .await?;
        let pr = &data["repository"]["pullRequest"];
        let head = string(pr, "headRefOid")?.to_owned();
        ensure!(
            !head.is_empty() && head.len() <= 64 && head.bytes().all(|b| b.is_ascii_hexdigit()),
            "Invalid PR revision"
        );
        let stamp = string(pr, "updatedAt")?.to_owned();
        if revision.is_some() {
            ensure!(
                revision.as_ref() == Some(&head) && updated.as_ref() == Some(&stamp),
                "PR changed during fetch; refresh reviews"
            );
        }
        revision = Some(head);
        updated = Some(stamp);
        let connection = &pr["reviewThreads"];
        for raw in nodes(connection)? {
            let mut comments = nodes(&raw["comments"])?.clone();
            let mut reply_bytes = serde_json::to_vec(&comments)?.len();
            let mut after = cursor(&raw["comments"])?;
            for page in 0..MAX_PAGES {
                let Some(next) = after else {
                    break;
                };
                let data = api(
                    transport,
                    COMMENTS,
                    json!({"id":string(raw,"id")?,"after":next}),
                )
                .await?;
                let connection = &data["node"]["comments"];
                let page_comments = nodes(connection)?;
                reply_bytes += serde_json::to_vec(page_comments)?.len();
                ensure!(
                    reply_bytes <= LIMIT as usize,
                    "Discussion exceeds reply size limit"
                );
                comments.extend(page_comments.iter().cloned());
                after = cursor(connection)?;
                ensure!(
                    after.is_none() || page + 1 < MAX_PAGES,
                    "Too many replies to load safely"
                );
            }
            let thread = parse_thread(raw, comments)?;
            content_bytes += thread
                .comments
                .iter()
                .map(|c| c.body.len() + c.author.len())
                .sum::<usize>()
                + thread.diff.len();
            ensure!(
                content_bytes <= LIMIT as usize && threads.len() < 2000,
                "Review exceeds display size limit"
            );
            threads.push(Arc::new(thread));
        }
        after = cursor(connection)?;
        if after.is_none() {
            break;
        }
        ensure!(
            page + 1 < MAX_PAGES,
            "Too many review threads to load safely"
        );
    }
    let revision = revision.unwrap();
    let mut sources = HashMap::new();
    let mut visited = HashSet::new();
    for thread in &threads {
        if thread.lines.is_none() || !visited.insert(&thread.path) {
            continue;
        }
        // Immutable PR head blob, never the worktree/index or the diff's old side.
        let expression = format!("{revision}:{}", thread.path);
        let data = api(transport, "query($owner:String!,$repo:String!,$expression:String!) { repository(owner:$owner,name:$repo) { object(expression:$expression) { ... on Blob { text isBinary byteSize } } } }", json!({"owner":owner,"repo":repo,"expression":expression})).await?;
        let blob = &data["repository"]["object"];
        if blob["isBinary"] == false
            && blob["byteSize"]
                .as_u64()
                .is_some_and(|n| n <= 2 * 1024 * 1024)
        {
            if let Some(text) = blob["text"].as_str() {
                content_bytes += text.len();
                ensure!(
                    content_bytes <= LIMIT as usize,
                    "Review source snapshots exceed size limit"
                );
                sources.insert(thread.path.clone(), text.to_owned());
            }
        }
    }
    let data = api(transport, "query($owner:String!,$repo:String!,$number:Int!) { repository(owner:$owner,name:$repo) { pullRequest(number:$number) { headRefOid updatedAt } } }", json!({"owner":owner,"repo":repo,"number":pull.number})).await?;
    let pr = &data["repository"]["pullRequest"];
    ensure!(
        pr["headRefOid"] == revision && pr["updatedAt"].as_str() == updated.as_deref(),
        "PR changed during fetch; refresh reviews"
    );
    threads.sort_by(|a, b| {
        (&a.path, a.lines.as_ref().map(|r| r.start), &a.id).cmp(&(
            &b.path,
            b.lines.as_ref().map(|r| r.start),
            &b.id,
        ))
    });
    Ok(Some(Review {
        label: format!(
            "{}#{} @ {}",
            pull.repo,
            pull.number,
            &revision[..revision.len().min(12)]
        ),
        threads,
        sources,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn no_pull_is_graceful_and_multiple_pulls_are_explicit() {
        assert!(unique_pull(vec![]).unwrap().is_none());
        let pull = Pull {
            repo: "o/r".into(),
            number: 1,
            url: "https://github.com/o/r/pull/1".into(),
        };
        assert_eq!(unique_pull(vec![pull.clone()]).unwrap().unwrap().number, 1);
        assert!(unique_pull(vec![pull.clone(), pull])
            .unwrap_err()
            .to_string()
            .contains("Ambiguous branch association"));
    }

    #[test]
    fn triangular_fork_uses_push_identity() {
        let repos = HashMap::from([
            ("origin".into(), "me/fork".into()),
            ("upstream".into(), "org/repo".into()),
        ]);
        assert_eq!(
            head_target(&repos, "feature", Some("upstream"), Some("origin"), None).unwrap(),
            ("me/fork".into(), "feature".into())
        );
        assert_eq!(
            head_target(&repos, "feature", Some("upstream"), None, None).unwrap(),
            ("me/fork".into(), "feature".into())
        );
        assert_eq!(
            head_target(
                &repos,
                "local-alias",
                Some("origin"),
                None,
                Some("refs/remotes/origin/topic")
            )
            .unwrap(),
            ("me/fork".into(), "topic".into())
        );
        assert!(head_target(&repos, "feature", None, Some("unsupported"), None).is_err());
    }

    #[test]
    fn paths_and_remotes() {
        for path in [
            "../secret",
            "/etc/passwd",
            "a/../../b",
            "a\\b",
            "C:/x",
            "a/.git/config",
            "a//b",
            "./b",
            "x\x1b[31m",
        ] {
            assert!(!valid_path(path), "{path:?}");
        }
        assert!(valid_path("src/a file.rs"));
        assert_eq!(
            repository("git@github.com:owner/repo.git"),
            Some("owner/repo".into())
        );
        assert!(repository("https://evil.example/owner/repo").is_none());
    }
    #[test]
    fn only_current_right_side_is_anchorable() {
        let mut raw = json!({"id":"thread","path":"a.rs","line":3,"startLine":2,"diffSide":"RIGHT","isOutdated":false,"isResolved":true});
        let comments = vec![
            json!({"author":{"login":"a"},"body":"hello\u{001b}","diffHunk":"@@ old","url":"url"}),
        ];
        assert_eq!(
            parse_thread(&raw, comments.clone()).unwrap().lines,
            Some(2..4)
        );
        raw["diffSide"] = json!("LEFT");
        assert!(parse_thread(&raw, comments.clone())
            .unwrap()
            .lines
            .is_none());
        raw["diffSide"] = json!("RIGHT");
        raw["isOutdated"] = json!(true);
        assert!(parse_thread(&raw, comments.clone())
            .unwrap()
            .lines
            .is_none());
        raw["isOutdated"] = json!(false);
        raw["path"] = json!("../x");
        assert!(parse_thread(&raw, comments).unwrap().lines.is_none());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn process_arguments_are_not_shell_code() {
        let dir = tempfile::tempdir().unwrap();
        let payload = "$(touch injected); `touch injected`\n--evil";
        let result = run(dir.path(), "printf", &["%s", payload], None)
            .await
            .unwrap();
        assert_eq!(result, payload);
        assert!(!dir.path().join("injected").exists());
    }
}

#[cfg(all(test, unix))]
mod transport_tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};

    fn connection(nodes: Vec<Value>, cursor: Option<&str>) -> Value {
        json!({"nodes":nodes,"pageInfo":{"hasNextPage":cursor.is_some(),"endCursor":cursor}})
    }
    fn output(value: Value) -> String {
        format!(
            "printf '%s' '{}'",
            json!({"data":value}).to_string().replace('\'', "'\\''")
        )
    }
    fn script(changed: bool) -> String {
        let comment = json!({"author":{"login":"alice"},"body":"first\u{001b}[31m","diffHunk":"@@ -1 +1 @@\n-old\n+new","url":"https://github.com/o/r/pull/1#discussion_r1","originalCommit":{"oid":"old"}});
        let reply = json!({"author":{"login":"bob"},"body":"reply","diffHunk":"","url":""});
        let raw = json!({"id":"t1","path":"a.rs","line":1,"diffSide":"RIGHT","isOutdated":false,"isResolved":true,"comments":connection(vec![comment],Some("replies"))});
        let pr = |threads| json!({"repository":{"pullRequest":{"headRefOid":"abc123","updatedAt":"now","reviewThreads":threads}}});
        format!("#!/bin/sh\ninput=$(cat)\ncase \"$input\" in\n*reviewThreads*)\n case \"$input\" in\n *'\"after\":\"threads\"'*) {};;\n *) {};;\n esac;;\n*PullRequestReviewThread*) {};;\n*byteSize*) {};;\n*) {};;\nesac\n",
            output(pr(connection(vec![],None))),
            output(pr(connection(vec![raw],Some("threads")))),
            output(json!({"node":{"comments":connection(vec![reply],None)}})),
            output(json!({"repository":{"object":{"text":"new\n","isBinary":false,"byteSize":4}}})),
            output(json!({"repository":{"pullRequest":{"headRefOid":if changed { "different" } else { "abc123" },"updatedAt":"now"}}})))
    }
    #[tokio::test]
    async fn fake_discovery_uses_pushurl_and_handles_absent_and_ambiguous_prs() {
        let dir = tempfile::tempdir().unwrap();
        let context = Context {
            root: dir.path().into(),
            git_dir: dir.path().join(".git"),
            branch: "feature".into(),
            head: "abc123".into(),
            config: vec![],
        };
        let init = std::process::Command::new("git")
            .args(["init", "--quiet", "--initial-branch=feature"])
            .arg(dir.path())
            .status()
            .unwrap();
        assert!(init.success());
        fs::write(dir.path().join(".git/config"), "[core]\nrepositoryformatversion = 0\n[remote \"origin\"]\nurl = https://github.com/org/repo.git\npushurl = https://github.com/me/fork.git\nfetch = +refs/heads/*:refs/remotes/origin/*\n").unwrap();
        let gh = dir.path().join("fake-gh");
        let transport = Transport {
            context: &context,
            gh: gh.to_str().unwrap(),
        };
        let pull = json!({"number":3,"url":"https://github.com/org/repo/pull/3","headRefName":"feature","headRepository":{"nameWithOwner":"me/fork"}});
        for mode in 0..3 {
            let matched = if mode == 0 {
                vec![]
            } else {
                vec![pull.clone()]
            };
            let fork = if mode == 2 {
                vec![pull.clone()]
            } else {
                vec![]
            };
            let response =
                |items| output(json!({"repository":{"pullRequests":connection(items,None)}}));
            let script = format!("#!/bin/sh\ninput=$(cat)\ncase \"$input\" in\n*pullRequests*)\n case \"$input\" in\n *'\"repo\":\"fork\"'*) {};;\n *) {};;\n esac;;\n*) {};;\nesac\n", response(fork), response(matched), output(json!({"repository":{"parent":{"nameWithOwner":"org/repo"}}})));
            fs::write(&gh, script).unwrap();
            fs::set_permissions(&gh, fs::Permissions::from_mode(0o700)).unwrap();
            let result = discover(&transport).await;
            match mode {
                0 => assert!(result.unwrap().is_none()),
                1 => assert_eq!(result.unwrap().unwrap().number, 3),
                _ => assert!(result.unwrap_err().to_string().contains("Ambiguous")),
            }
        }
    }

    #[tokio::test]
    async fn fake_cli_paginates_threads_replies_and_rejects_revision_races() {
        let dir = tempfile::tempdir().unwrap();
        let gh = dir.path().join("fake-gh");
        let context = Context {
            root: dir.path().into(),
            git_dir: dir.path().join(".git"),
            branch: "topic".into(),
            head: "abc123".into(),
            config: vec![],
        };
        fs::write(&gh, script(false)).unwrap();
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o700)).unwrap();
        let transport = Transport {
            context: &context,
            gh: gh.to_str().unwrap(),
        };
        let review = fetch_with(&transport, Some("o/r#1".into()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(review.threads.len(), 1);
        assert_eq!(review.threads[0].comments.len(), 2);
        assert_eq!(review.threads[0].comments[0].body, "first[31m");
        assert!(review.threads[0].resolved);
        assert_eq!(review.sources["a.rs"], "new\n");
        fs::write(&gh, script(true)).unwrap();
        assert!(fetch_with(&transport, Some("o/r#1".into()))
            .await
            .unwrap_err()
            .to_string()
            .contains("PR changed"));
    }
}
