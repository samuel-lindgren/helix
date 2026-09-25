//! GitHub CLI transport. Authentication belongs to gh. Writes are limited to
//! replying to and (un)resolving an existing review discussion, addressed by its
//! node id. All user and remote text travels as JSON variables on stdin, never
//! in the query, a shell or command options.
use anyhow::{anyhow, bail, ensure};
use helix_view::review::{safe_text, Comment, Context, Review, Thread};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    path::{Component, Path},
    sync::Arc,
    time::Duration,
};

use crate::process::LIMIT;
const MAX_PAGES: usize = 100;

/// Every process is bounded by its own timeout; see [`crate::process`].
const TIMEOUT: Duration = Duration::from_secs(30);

async fn run(
    root: &Path,
    program: &str,
    args: &[&str],
    input: Option<Vec<u8>>,
) -> anyhow::Result<String> {
    crate::process::run(root, program, args, input, TIMEOUT).await
}

pub(super) async fn git(context: &Context, args: &[&str]) -> anyhow::Result<String> {
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
    if let Some(errors) = value.get("errors") {
        bail!("GitHub query failed: {}", graphql_errors(errors));
    }
    value
        .get("data")
        .cloned()
        .ok_or_else(|| anyhow!("GitHub returned no data"))
}

/// Messages only (not the whole error JSON), bounded and terminal-safe.
fn graphql_errors(errors: &Value) -> String {
    let messages: Vec<_> = errors
        .as_array()
        .map(|errors| {
            errors
                .iter()
                .filter_map(|e| e["message"].as_str())
                .collect()
        })
        .unwrap_or_default();
    let text = if messages.is_empty() {
        errors.to_string()
    } else {
        messages.join("; ")
    };
    safe_text(&text).chars().take(500).collect()
}

/// Mutations fail most often for access reasons; say what to check.
fn write_error(action: &str, err: anyhow::Error) -> anyhow::Error {
    let text = format!("{err:#}");
    let lower = text.to_ascii_lowercase();
    let hint = if [
        "not accessible",
        "forbidden",
        "permission",
        "scope",
        "must have",
        "http 403",
    ]
    .iter()
    .any(|s| lower.contains(s))
    {
        " (needs write access to the repository and a gh token with the repo scope; check `gh auth status`)"
    } else {
        ""
    };
    anyhow!("{action} failed: {text}{hint}")
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

/// `owner/repo` of a github.com remote URL.
pub(crate) fn repository(url: &str) -> Option<String> {
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
    /// Head repository, when discovered from the branch.
    head_repo: Option<String>,
}

const DISCOVER: &str = r#"query($owner:String!,$repo:String!,$branch:String!,$after:String) {
 repository(owner:$owner,name:$repo) { pullRequests(headRefName:$branch,states:OPEN,first:100,after:$after) {
 nodes { number url headRefName headRepository { nameWithOwner } } pageInfo { hasNextPage endCursor }
 } } }"#;

/// Push identity matters in triangular forks: a feature may fetch upstream/main
/// while pushing origin/feature. Never substitute its fetch-upstream branch.
///
/// Returns the head repository and the branch names to try, in order. When Git
/// cannot resolve `@{push}` (for example `push.default=simple` with a local name
/// that differs from its upstream), the upstream branch name is tried after the
/// local name, but only if the upstream lives in the head repository itself.
fn head_target(
    repos: &HashMap<String, String>,
    branch: &str,
    tracking: Option<&str>,
    merge: Option<&str>,
    push_remote: Option<&str>,
    push_ref: Option<&str>,
) -> anyhow::Result<(String, Vec<String>)> {
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
            return Ok(((*repo).clone(), vec![(*branch).to_owned()]));
        }
    }
    let (remote, repo) = if let Some(remote) = push_remote {
        let repo = repos.get(remote).ok_or_else(|| {
            anyhow!("Configured push remote is not a supported GitHub repository")
        })?;
        (remote, repo)
    } else {
        ["origin"]
            .into_iter()
            .chain(tracking)
            .find_map(|remote| repos.get_key_value(remote))
            .map(|(remote, repo)| (remote.as_str(), repo))
            .or_else(|| {
                (repos.len() == 1)
                    .then(|| repos.iter().next().map(|(r, repo)| (r.as_str(), repo)))
                    .flatten()
            })
            .ok_or_else(|| {
                anyhow!(
                    "Cannot identify GitHub head repository; use :review-select owner/repo#number"
                )
            })?
    };
    let mut branches = vec![branch.to_owned()];
    if tracking == Some(remote) {
        if let Some(upstream) = merge
            .and_then(|m| m.strip_prefix("refs/heads/"))
            .filter(|b| !b.is_empty() && *b != branch)
        {
            branches.push(upstream.to_owned());
        }
    }
    Ok((repo.clone(), branches))
}

async fn find_pulls(
    transport: &Transport<'_>,
    bases: &HashSet<String>,
    head_repo: &str,
    branch: &str,
) -> anyhow::Result<Vec<Pull>> {
    // Own fork and its parent can both have a PR for the same branch. Never pick
    // the first match. Discovery must finish in every candidate base repository.
    let mut pulls = Vec::new();
    for base in bases {
        let (owner, repo) = names(base)?;
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
                    .is_some_and(|r| r.eq_ignore_ascii_case(head_repo))
                    && pr["headRefName"] == branch
                {
                    pulls.push(Pull {
                        repo: base.clone(),
                        number: pr["number"]
                            .as_u64()
                            .ok_or_else(|| anyhow!("Missing PR number"))?,
                        url: safe_text(string(pr, "url")?),
                        head_repo: Some(head_repo.to_owned()),
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
    Ok(pulls)
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
    ensure!(
        !repos.is_empty(),
        "No github.com remote found; use :review-select owner/repo#number"
    );
    let config = |key: String| async move { git(context, &["config", "--get", &key]).await.ok() };
    let tracking = config(format!("branch.{}.remote", context.branch)).await;
    let merge = config(format!("branch.{}.merge", context.branch)).await;
    let push_remote = match config(format!("branch.{}.pushRemote", context.branch)).await {
        Some(remote) => Some(remote),
        None => config("remote.pushDefault".into()).await,
    };
    let push_ref = git(context, &["rev-parse", "--symbolic-full-name", "@{push}"])
        .await
        .ok();
    let (head_repo, branches) = head_target(
        &repos,
        &context.branch,
        tracking.as_deref(),
        merge.as_deref(),
        push_remote.as_deref(),
        push_ref.as_deref(),
    )?;
    let (owner, name) = names(&head_repo)?;
    let parent = api(transport, "query($owner:String!,$repo:String!) { repository(owner:$owner,name:$repo) { parent { nameWithOwner } } }", json!({"owner":owner,"repo":name})).await?;
    if let Some(parent) = parent["repository"]["parent"]["nameWithOwner"].as_str() {
        bases.insert(parent.to_owned());
    }
    for branch in &branches {
        let pulls = find_pulls(transport, &bases, &head_repo, branch).await?;
        if !pulls.is_empty() {
            return unique_pull(pulls);
        }
    }
    Ok(None)
}

fn unique_pull(mut pulls: Vec<Pull>) -> anyhow::Result<Option<Pull>> {
    ensure!(pulls.len() <= 1, "Ambiguous branch association: {}. Use :review-select owner/repo#number to choose for this context.", pulls.iter().map(|p| p.url.as_str()).collect::<Vec<_>>().join(", "));
    Ok(pulls.pop())
}

macro_rules! thread_fields {
    () => {
        " id path line startLine diffSide startDiffSide originalLine originalStartLine isOutdated isResolved
 viewerCanReply viewerCanResolve viewerCanUnresolve
 comments(first:100) { nodes { author { login } body diffHunk url originalCommit { oid } } pageInfo { hasNextPage endCursor } } "
    };
}
const THREADS: &str = concat!(
    "query($owner:String!,$repo:String!,$number:Int!,$after:String) {
 repository(owner:$owner,name:$repo) { pullRequest(number:$number) { headRefOid updatedAt url
 reviewThreads(first:100,after:$after) { nodes {",
    thread_fields!(),
    "} pageInfo { hasNextPage endCursor } } } } }"
);
const THREAD: &str = concat!(
    "query($id:ID!) { node(id:$id) { ... on PullRequestReviewThread { pullRequest { headRefOid }",
    thread_fields!(),
    "} } }"
);
const PULL_HEAD: &str = "query($owner:String!,$repo:String!,$number:Int!) { repository(owner:$owner,name:$repo) {
 pullRequest(number:$number) { headRefOid headRefName baseRefOid headRepository { nameWithOwner } } } }";
const REPLY: &str = "mutation($id:ID!,$body:String!) { addPullRequestReviewThreadReply(input:{pullRequestReviewThreadId:$id,body:$body}) { comment { url } } }";
const RESOLVE: &str =
    "mutation($id:ID!) { resolveReviewThread(input:{threadId:$id}) { thread { isResolved } } }";
const UNRESOLVE: &str =
    "mutation($id:ID!) { unresolveReviewThread(input:{threadId:$id}) { thread { isResolved } } }";
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
    let span = |start: &Value, end: &Value| match (start.as_u64(), end.as_u64()) {
        (Some(start), Some(end)) if start != end => format!("{start}–{end}"),
        (_, Some(end)) => end.to_string(),
        _ => "?".into(),
    };
    let current = span(&json!(start), &json!(line));
    let original = span(&value["originalStartLine"], &value["originalLine"]);
    let commit = first["originalCommit"]["oid"].as_str().unwrap_or("unknown");
    let location = format!(
        "{}:{current} ({status}, {}, original {original}, commit {})",
        safe_text(path),
        safe_text(value["diffSide"].as_str().unwrap_or("?")),
        safe_text(&commit[..commit.len().min(12)])
    );
    let original_commit = first["originalCommit"]["oid"]
        .as_str()
        .filter(|oid| valid_oid(oid))
        .map(str::to_owned);
    let original_line = value["originalLine"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok());
    let original_start = value["originalStartLine"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .or(original_line);
    let original_lines = if valid_path(path) && right {
        original_start
            .zip(original_line)
            .filter(|(s, e)| *s > 0 && s <= e)
            .and_then(|(s, e)| e.checked_add(1).map(|end| s..end))
    } else {
        None
    };
    Ok(Thread {
        id: string(value, "id")?.to_owned(),
        path: path.to_owned(),
        lines,
        location,
        resolved: value["isResolved"].as_bool().unwrap_or(false),
        outdated,
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
        commit: original_commit,
        original_lines,
        can_reply: value["viewerCanReply"].as_bool().unwrap_or(false),
        can_resolve: value["viewerCanResolve"].as_bool().unwrap_or(false),
        can_unresolve: value["viewerCanUnresolve"].as_bool().unwrap_or(false),
    })
}

pub(super) fn valid_oid(oid: &str) -> bool {
    (4..=64).contains(&oid.len()) && oid.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Follow a discussion's reply pages, bounded like the review fetch.
async fn thread_comments(transport: &Transport<'_>, raw: &Value) -> anyhow::Result<Vec<Value>> {
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
    Ok(comments)
}

/// The current PR head, fetched fresh for every write-side check: a push after
/// the review loaded must count.
#[derive(Clone, Debug)]
pub(crate) struct PullHead {
    pub oid: String,
    /// `owner/repo:branch`, for messages.
    pub name: String,
    pub base: Option<String>,
}

pub(crate) async fn pull_head(
    context: &Context,
    repo: &str,
    number: u64,
) -> anyhow::Result<PullHead> {
    pull_head_with(&Transport { context, gh: &gh() }, repo, number).await
}

async fn pull_head_with(
    transport: &Transport<'_>,
    repo: &str,
    number: u64,
) -> anyhow::Result<PullHead> {
    let (owner, name) = names(repo)?;
    let data = api(
        transport,
        PULL_HEAD,
        json!({"owner":owner,"repo":name,"number":number}),
    )
    .await?;
    let pr = &data["repository"]["pullRequest"];
    let oid = string(pr, "headRefOid")?;
    ensure!(valid_oid(oid), "Invalid PR revision");
    Ok(PullHead {
        oid: oid.to_owned(),
        name: safe_text(&format!(
            "{}:{}",
            pr["headRepository"]["nameWithOwner"]
                .as_str()
                .unwrap_or("[deleted]"),
            pr["headRefName"].as_str().unwrap_or("?")
        )),
        base: pr["baseRefOid"]
            .as_str()
            .filter(|oid| valid_oid(oid))
            .map(str::to_owned),
    })
}

/// Reload one discussion after a write, with the PR head its lines belong to.
pub(super) async fn fetch_thread(context: &Context, id: &str) -> anyhow::Result<(Thread, String)> {
    fetch_thread_with(&Transport { context, gh: &gh() }, id).await
}

async fn fetch_thread_with(
    transport: &Transport<'_>,
    id: &str,
) -> anyhow::Result<(Thread, String)> {
    let data = api(transport, THREAD, json!({ "id": id })).await?;
    let raw = &data["node"];
    let head = string(&raw["pullRequest"], "headRefOid")?;
    ensure!(valid_oid(head), "Invalid PR revision");
    let comments = thread_comments(transport, raw).await?;
    Ok((parse_thread(raw, comments)?, head.to_owned()))
}

/// Post `body` as a reply in discussion `id`; returns the new comment's URL.
pub(super) async fn reply(context: &Context, id: &str, body: &str) -> anyhow::Result<String> {
    reply_with(&Transport { context, gh: &gh() }, id, body).await
}

async fn reply_with(transport: &Transport<'_>, id: &str, body: &str) -> anyhow::Result<String> {
    ensure!(!body.trim().is_empty(), "Reply is empty");
    ensure!(
        body.len() <= 65536,
        "Reply exceeds GitHub's comment size limit"
    );
    let data = api(transport, REPLY, json!({"id":id,"body":body}))
        .await
        .map_err(|err| write_error("Reply", err))?;
    let url = data["addPullRequestReviewThreadReply"]["comment"]["url"]
        .as_str()
        .ok_or_else(|| anyhow!("Reply failed: GitHub returned no comment"))?;
    Ok(safe_text(url))
}

/// Resolve (`true`) or unresolve a discussion; returns the new resolved state.
pub(super) async fn set_resolved(
    context: &Context,
    id: &str,
    resolved: bool,
) -> anyhow::Result<bool> {
    set_resolved_with(&Transport { context, gh: &gh() }, id, resolved).await
}

async fn set_resolved_with(
    transport: &Transport<'_>,
    id: &str,
    resolved: bool,
) -> anyhow::Result<bool> {
    let (query, field, action) = if resolved {
        (RESOLVE, "resolveReviewThread", "Resolve")
    } else {
        (UNRESOLVE, "unresolveReviewThread", "Unresolve")
    };
    let data = api(transport, query, json!({ "id": id }))
        .await
        .map_err(|err| write_error(action, err))?;
    data[field]["thread"]["isResolved"]
        .as_bool()
        .ok_or_else(|| anyhow!("{action} failed: GitHub returned no thread state"))
}

/// Unit tests substitute a recording fake for the GitHub CLI program. Tests
/// that set it hold `TEST_GH_LOCK` until they finish.
#[cfg(test)]
pub(crate) static TEST_GH: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
#[cfg(test)]
pub(crate) static TEST_GH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn gh() -> String {
    #[cfg(test)]
    if let Some(gh) = TEST_GH.lock().unwrap().clone() {
        return gh;
    }
    "gh".into()
}

pub(super) async fn fetch(
    context: &Context,
    selection: Option<String>,
) -> anyhow::Result<Option<Review>> {
    fetch_with(&Transport { context, gh: &gh() }, selection).await
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
            head_repo: None,
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
            let comments = thread_comments(transport, raw).await?;
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
    let originals = originals(transport.context, &threads, &mut content_bytes).await;
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
        repo: pull.repo,
        number: pull.number,
        head: revision,
        threads,
        sources,
        originals,
        head_repo: pull.head_repo,
    }))
}

/// File contents at the original commit of each outdated discussion, from the
/// local repository only (commits and blobs are immutable). Missing commits,
/// for example after a rebase, leave the discussion at its original line numbers.
async fn originals(
    context: &Context,
    threads: &[Arc<Thread>],
    content_bytes: &mut usize,
) -> HashMap<(String, String), String> {
    let mut originals = HashMap::new();
    let mut visited = HashSet::new();
    for thread in threads {
        let (None, Some(_), Some(commit)) = (&thread.lines, &thread.original_lines, &thread.commit)
        else {
            continue;
        };
        let key = (commit.clone(), thread.path.clone());
        if !valid_path(&thread.path) || !valid_oid(commit) || !visited.insert(key.clone()) {
            continue;
        }
        let size = run(
            &context.root,
            "git",
            &["cat-file", "-s", &format!("{commit}:{}", thread.path)],
            None,
        )
        .await;
        if !size.is_ok_and(|size| {
            size.trim()
                .parse::<usize>()
                .is_ok_and(|n| n <= 2 * 1024 * 1024)
        }) {
            continue;
        }
        let Ok(text) = run(
            &context.root,
            "git",
            &["cat-file", "blob", &format!("{commit}:{}", thread.path)],
            None,
        )
        .await
        else {
            continue;
        };
        if *content_bytes + text.len() > LIMIT as usize {
            break;
        }
        *content_bytes += text.len();
        originals.insert(key, text);
    }
    originals
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
            head_repo: None,
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
        let target = |branch, tracking, merge, push_remote, push_ref| {
            head_target(&repos, branch, tracking, merge, push_remote, push_ref)
        };
        let expect = |repo: &str, branches: &[&str]| {
            (
                repo.to_owned(),
                branches.iter().map(|b| b.to_string()).collect::<Vec<_>>(),
            )
        };
        assert_eq!(
            target(
                "feature",
                Some("upstream"),
                Some("refs/heads/main"),
                Some("origin"),
                None
            )
            .unwrap(),
            expect("me/fork", &["feature"])
        );
        assert_eq!(
            target(
                "feature",
                Some("upstream"),
                Some("refs/heads/main"),
                None,
                None
            )
            .unwrap(),
            expect("me/fork", &["feature"])
        );
        assert_eq!(
            target(
                "local-alias",
                Some("origin"),
                Some("refs/heads/topic"),
                None,
                Some("refs/remotes/origin/topic")
            )
            .unwrap(),
            expect("me/fork", &["topic"])
        );
        assert!(target("feature", None, None, Some("unsupported"), None).is_err());
    }

    /// push.default=simple cannot resolve @{push} when the local name differs
    /// from its upstream. The upstream name is a fallback within the same remote.
    #[test]
    fn renamed_local_branch_falls_back_to_upstream_name() {
        let repos = HashMap::from([
            ("origin".into(), "me/fork".into()),
            ("upstream".into(), "org/repo".into()),
        ]);
        assert_eq!(
            head_target(
                &repos,
                "local-alias",
                Some("origin"),
                Some("refs/heads/topic"),
                None,
                None
            )
            .unwrap(),
            (
                "me/fork".to_owned(),
                vec!["local-alias".to_owned(), "topic".to_owned()]
            )
        );
        // Tracking another repository's branch never changes the head branch.
        assert_eq!(
            head_target(
                &repos,
                "feature",
                Some("upstream"),
                Some("refs/heads/main"),
                None,
                None
            )
            .unwrap()
            .1,
            vec!["feature".to_owned()]
        );
        let single = HashMap::from([("github".into(), "o/r".into())]);
        assert_eq!(
            head_target(
                &single,
                "mine",
                Some("github"),
                Some("refs/heads/SS-1-topic"),
                None,
                None
            )
            .unwrap(),
            (
                "o/r".to_owned(),
                vec!["mine".to_owned(), "SS-1-topic".to_owned()]
            )
        );
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
    async fn fake_discovery_finds_pr_for_renamed_local_branch() {
        let dir = tempfile::tempdir().unwrap();
        let context = Context {
            root: dir.path().into(),
            git_dir: dir.path().join(".git"),
            branch: "local-alias".into(),
            head: "abc123".into(),
            config: vec![],
        };
        let init = std::process::Command::new("git")
            .args(["init", "--quiet", "--initial-branch=local-alias"])
            .arg(dir.path())
            .status()
            .unwrap();
        assert!(init.success());
        // No refs exist, so @{push} cannot resolve, as with push.default=simple.
        fs::write(dir.path().join(".git/config"), "[core]\nrepositoryformatversion = 0\n[remote \"origin\"]\nurl = git@github.com:org/repo.git\nfetch = +refs/heads/*:refs/remotes/origin/*\n[branch \"local-alias\"]\nremote = origin\nmerge = refs/heads/feature\n").unwrap();
        let gh = dir.path().join("fake-gh");
        let pull = json!({"number":7,"url":"https://github.com/org/repo/pull/7","headRefName":"feature","headRepository":{"nameWithOwner":"org/repo"}});
        let response =
            |items| output(json!({"repository":{"pullRequests":connection(items,None)}}));
        let script = format!("#!/bin/sh\ninput=$(cat)\ncase \"$input\" in\n*'\"branch\":\"feature\"'*) {};;\n*pullRequests*) {};;\n*) {};;\nesac\n", response(vec![pull]), response(vec![]), output(json!({"repository":{"parent":null}})));
        fs::write(&gh, script).unwrap();
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o700)).unwrap();
        let transport = Transport {
            context: &context,
            gh: gh.to_str().unwrap(),
        };
        let pull = discover(&transport).await.unwrap().unwrap();
        assert_eq!((pull.repo.as_str(), pull.number), ("org/repo", 7));
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

    #[tokio::test]
    async fn outdated_sources_come_from_local_history_only() {
        let dir = tempfile::tempdir().unwrap();
        let sh = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
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
        };
        sh(&["init", "--quiet", "--initial-branch=topic"]);
        fs::write(dir.path().join("a.rs"), "one\ntwo\n").unwrap();
        sh(&["add", "a.rs"]);
        sh(&["commit", "--quiet", "-m", "reviewed"]);
        let sha = sh(&["rev-parse", "HEAD"]);
        let context = Context {
            root: dir.path().into(),
            git_dir: dir.path().join(".git"),
            branch: "topic".into(),
            head: sha.clone(),
            config: vec![],
        };
        let thread = |path: &str, lines, commit: &str| {
            Arc::new(Thread {
                path: path.into(),
                lines,
                original_lines: Some(1..2),
                commit: Some(commit.into()),
                outdated: true,
                ..Default::default()
            })
        };
        let threads = [
            thread("a.rs", None, &sha),
            thread("a.rs", None, &sha),
            thread("a.rs", None, "0123abcd"),
            thread("missing.rs", None, &sha),
            thread("../a.rs", None, &sha),
            thread("a.rs", Some(1..2), &sha),
        ];
        let mut bytes = 0;
        let originals = originals(&context, &threads, &mut bytes).await;
        assert_eq!(originals.len(), 1);
        assert_eq!(originals[&(sha, "a.rs".to_owned())], "one\ntwo\n");
        assert_eq!(bytes, 8);
    }

    /// Records every request; answers mutations and the single-thread reload.
    fn write_script(log: &std::path::Path, fail: bool) -> String {
        let thread = json!({"node":{"pullRequest":{"headRefOid":"abc123"},"id":"T1","path":"a.rs","line":1,"diffSide":"RIGHT","isOutdated":false,"isResolved":true,
            "viewerCanReply":true,"viewerCanResolve":false,"viewerCanUnresolve":true,
            "comments":connection(vec![
                json!({"author":{"login":"alice"},"body":"question","diffHunk":"","url":"u","originalCommit":{"oid":"0123abcd"}}),
                json!({"author":{"login":"me"},"body":"answer","diffHunk":"","url":"u2"}),
            ],None)}});
        let fail = if fail {
            "*addPullRequestReviewThreadReply*|*resolveReviewThread*) echo 'gh: Resource not accessible by integration (HTTP 403)' >&2; exit 1;;\n"
        } else {
            ""
        };
        format!(
            "#!/bin/sh\ninput=$(cat)\nprintf '%s\\n' \"$input\" >> '{}'\ncase \"$input\" in\n{fail}*addPullRequestReviewThreadReply*) {};;\n*unresolveReviewThread*) {};;\n*resolveReviewThread*) {};;\n*headRefName*) {};;\n*PullRequestReviewThread*) {};;\n*) exit 3;;\nesac\n",
            log.display(),
            output(json!({"addPullRequestReviewThreadReply":{"comment":{"url":"https://github.com/o/r/pull/1#discussion_r2"}}})),
            output(json!({"unresolveReviewThread":{"thread":{"isResolved":false}}})),
            output(json!({"resolveReviewThread":{"thread":{"isResolved":true}}})),
            output(json!({"repository":{"pullRequest":{"headRefOid":"abc123","headRefName":"topic","baseRefOid":"def456","headRepository":{"nameWithOwner":"me/fork"}}}})),
            output(thread),
        )
    }

    #[tokio::test]
    async fn fake_cli_receives_writes_as_json_variables() {
        let dir = tempfile::tempdir().unwrap();
        let gh = dir.path().join("fake-gh");
        let log = dir.path().join("requests");
        let context = Context {
            root: dir.path().into(),
            git_dir: dir.path().join(".git"),
            branch: "topic".into(),
            head: "abc123".into(),
            config: vec![],
        };
        fs::write(&gh, write_script(&log, false)).unwrap();
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o700)).unwrap();
        let transport = Transport {
            context: &context,
            gh: gh.to_str().unwrap(),
        };
        let body = "Fixed in 0123abcd.\n$(touch injected) `x` \"quoted\" 'single' }";
        let url = reply_with(&transport, "T1", body).await.unwrap();
        assert_eq!(url, "https://github.com/o/r/pull/1#discussion_r2");
        assert!(set_resolved_with(&transport, "T1", true).await.unwrap());
        assert!(!set_resolved_with(&transport, "T1", false).await.unwrap());
        let (thread, head) = fetch_thread_with(&transport, "T1").await.unwrap();
        assert_eq!(head, "abc123");
        assert_eq!(thread.comments.len(), 2);
        assert!(thread.resolved && thread.can_reply && thread.can_unresolve && !thread.can_resolve);
        assert_eq!(thread.commit.as_deref(), Some("0123abcd"));
        let pull = pull_head_with(&transport, "o/r", 1).await.unwrap();
        assert_eq!(
            (pull.oid.as_str(), pull.name.as_str(), pull.base.as_deref()),
            ("abc123", "me/fork:topic", Some("def456"))
        );
        assert!(reply_with(&transport, "T1", "  \n").await.is_err());

        let requests: Vec<Value> = fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(requests.len(), 5);
        let reply = &requests[0];
        assert!(reply["query"]
            .as_str()
            .unwrap()
            .contains("addPullRequestReviewThreadReply"));
        assert!(!reply["query"].as_str().unwrap().contains("Fixed"));
        assert_eq!(reply["variables"], json!({"id":"T1","body":body}));
        assert!(requests[1]["query"]
            .as_str()
            .unwrap()
            .contains("mutation($id:ID!) { resolveReviewThread"));
        assert!(requests[2]["query"]
            .as_str()
            .unwrap()
            .contains("unresolveReviewThread"));
        assert_eq!(requests[2]["variables"], json!({"id":"T1"}));
        assert!(!dir.path().join("injected").exists());

        // Access failures name the action and what to check.
        fs::write(&gh, write_script(&log, true)).unwrap();
        let err = reply_with(&transport, "T1", "hi")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("Reply failed"), "{err}");
        assert!(err.contains("gh auth status"), "{err}");
        let err = set_resolved_with(&transport, "T1", true)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.starts_with("Resolve failed") && err.contains("write access"),
            "{err}"
        );
        // GraphQL errors are reduced to their messages.
        assert_eq!(
            graphql_errors(
                &json!([{"type":"FORBIDDEN","message":"no\u{001b}[31m"},{"message":"two"}])
            ),
            "no[31m; two"
        );
    }
}
