//! Pull requests of the open project, fetched with `gh api graphql` in one query: review state per
//! reviewer, CI checks, merge queue, unresolved threads. PRs whose base is another listed PR's head
//! branch form a stack.

use crate::proc::run_bounded;
use serde_json::Value;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Scope {
    #[default]
    Mine,
    ReviewRequested,
}

/// Authentication or authorization errors that need the GitHub CLI setup flow.
pub fn needs_gh_login(error: &str) -> bool {
    let e = error.to_ascii_lowercase();
    ["auth login", "not logged in", "http 401", "http 403", "bad credentials", "resource not accessible", "authentication required", "must have admin rights", "saml"]
        .iter().any(|needle| e.contains(needle))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Review {
    Approved,
    ChangesRequested,
    Commented,
    Pending,
    Dismissed,
}

impl Review {
    fn parse(s: &str) -> Review {
        match s {
            "APPROVED" => Review::Approved,
            "CHANGES_REQUESTED" => Review::ChangesRequested,
            "DISMISSED" => Review::Dismissed,
            "PENDING" => Review::Pending,
            _ => Review::Commented,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Review::Approved => "Approved",
            Review::ChangesRequested => "Changes requested",
            Review::Commented => "Commented",
            Review::Pending => "Waiting",
            Review::Dismissed => "Dismissed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Ci {
    /// Ordered worst first, so sorting puts failures on top.
    Fail,
    Running,
    Pass,
    Skipped,
    None,
}

impl Ci {
    pub fn label(self) -> &'static str {
        match self {
            Ci::Fail => "Failing",
            Ci::Running => "Running",
            Ci::Pass => "Passing",
            Ci::Skipped => "Skipped",
            Ci::None => "No checks",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub state: Ci,
    pub url: String,
    /// Branch protection requires this check; `None` when GitHub did not say.
    pub required: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reviewer {
    pub name: String,
    pub state: Review,
    /// Unix seconds of the review; `None` while only requested.
    pub at: Option<i64>,
    /// Asked again after this review.
    pub re_requested: bool,
    pub team: bool,
    /// This reviewer is the authenticated `gh` user.
    pub mine: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pr {
    pub number: u64,
    /// `owner/name`: Review requested spans the whole organisation.
    pub repo: String,
    pub title: String,
    pub url: String,
    pub draft: bool,
    pub head: String,
    pub base: String,
    pub additions: u64,
    pub deletions: u64,
    pub files: u64,
    pub updated: i64,
    /// `APPROVED`, `CHANGES_REQUESTED`, `REVIEW_REQUIRED` or empty.
    pub decision: String,
    /// GitHub `mergeStateStatus`: CLEAN, BEHIND, DIRTY (conflicts), BLOCKED, UNSTABLE, ...
    pub merge_state: String,
    pub queue: Option<(u64, String)>,
    pub auto_merge: bool,
    pub reviewers: Vec<Reviewer>,
    pub unresolved: usize,
    pub ci: Ci,
    pub checks: Vec<Check>,
}

/// Deterministic fictional stack used by the PR screenshot scene.
pub fn demo_stack() -> (String, Vec<Pr>) {
    let now = crate::agentapp::now_ms() / 1000;
    let reviewer = |name: &str, state| Reviewer { name: name.into(), state, at: None, re_requested: false, team: false, mine: name == "you" };
    let mk = |number, title: &str, head: &str, base: &str, decision: &str, merge_state: &str, ci, reviewers, unresolved| Pr {
        number, repo: String::new(), title: title.into(), url: String::new(), draft: false,
        head: head.into(), base: base.into(), additions: 80, deletions: 12, files: 3, updated: now,
        decision: decision.into(), merge_state: merge_state.into(), queue: None, auto_merge: false,
        reviewers, unresolved, ci, checks: vec![],
    };
    let mut root = mk(204, "Add the search overview ruler", "feature/search-ruler", "main", "APPROVED", "CLEAN", Ci::Pass, vec![reviewer("you", Review::Approved), reviewer("reviewer-b", Review::Pending)], 1);
    root.additions = 164;
    root.checks = vec![Check { name: "unit-tests".into(), state: Ci::Pass, url: String::new(), required: Some(true) }, Check { name: "clippy".into(), state: Ci::Pass, url: String::new(), required: Some(false) }];
    let mut child = mk(205, "Support multiple search matches", "feature/multi-match", "feature/search-ruler", "REVIEW_REQUIRED", "BEHIND", Ci::Running, vec![reviewer("reviewer-a", Review::Pending)], 2);
    child.additions = 72;
    child.checks = vec![Check { name: "unit-tests".into(), state: Ci::Running, url: String::new(), required: Some(true) }];
    let leaf = mk(206, "Add keyboard shortcuts for sections", "feature/section-shortcuts", "feature/multi-match", "REVIEW_REQUIRED", "CLEAN", Ci::Pass, vec![reviewer("reviewer-a", Review::Pending)], 0);
    let mut draft = mk(207, "Polish compact PR cards", "feature/compact-cards", "main", "", "CLEAN", Ci::None, vec![], 0);
    draft.draft = true;
    ("codetrail (demo)".into(), vec![root, child, leaf, draft])
}

/// What the PR waits on, for the board's grouping and colours.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Phase {
    /// Changes requested, failing CI, or conflicts: the author has to act.
    NeedsWork,
    Queued,
    Ready,
    InReview,
    Draft,
}

impl Phase {
    pub const ALL: [Phase; 5] = [Phase::NeedsWork, Phase::InReview, Phase::Ready, Phase::Queued, Phase::Draft];
    pub fn label(self) -> &'static str {
        match self {
            Phase::NeedsWork => "Needs work",
            Phase::Queued => "In merge queue",
            Phase::Ready => "Ready to merge",
            Phase::InReview => "In review",
            Phase::Draft => "Draft",
        }
    }
}

impl Pr {
    pub fn phase(&self) -> Phase {
        if self.draft {
            Phase::Draft
        } else if self.queue.is_some() {
            Phase::Queued
        } else if self.decision == "CHANGES_REQUESTED" || self.ci == Ci::Fail || self.merge_state == "DIRTY" {
            Phase::NeedsWork
        } else if self.decision == "APPROVED" && matches!(self.ci, Ci::Pass | Ci::Skipped | Ci::None) {
            Phase::Ready
        } else {
            Phase::InReview
        }
    }

    pub fn approvals(&self) -> usize {
        self.reviewers.iter().filter(|r| r.state == Review::Approved && !r.re_requested).count()
    }

    /// Failing checks branch protection does not require (they do not hold the PR back).
    pub fn optional_failing(&self) -> usize {
        self.checks.iter().filter(|c| c.state == Ci::Fail && c.required == Some(false)).count()
    }

    pub fn merge_label(&self) -> Option<&'static str> {
        Some(match self.merge_state.as_str() {
            "DIRTY" => "Conflicts",
            "BEHIND" => "Behind base",
            "BLOCKED" => "Blocked",
            "UNSTABLE" => "Unstable",
            "CLEAN" | "HAS_HOOKS" => "Mergeable",
            _ => return None,
        })
    }
}

/// `2026-11-17T19:08:33Z` → unix seconds.
pub fn parse_time(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r).and_then(|x| x.parse::<i64>().ok());
    let (y, mo, d, h, mi, se) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    // days from civil (Howard Hinnant)
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (mo + if mo > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146_097 + doe - 719_468) * 86_400 + h * 3600 + mi * 60 + se)
}

fn ci_of(conclusion_or_state: &str, status: &str) -> Ci {
    match (conclusion_or_state, status) {
        (_, "QUEUED" | "IN_PROGRESS" | "WAITING" | "PENDING" | "REQUESTED") => Ci::Running,
        ("SUCCESS" | "NEUTRAL", _) => Ci::Pass,
        ("SKIPPED" | "STALE", _) => Ci::Skipped,
        ("FAILURE" | "ERROR" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE", _) => Ci::Fail,
        ("EXPECTED" | "PENDING", _) => Ci::Running,
        _ => Ci::Running,
    }
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

/// The checks of a `statusCheckRollup`, failures first. `isRequired` is read when the query asked
/// for it.
fn parse_checks(rollup: &Value) -> Vec<Check> {
    let mut checks: Vec<Check> = rollup["contexts"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            let required = c["isRequired"].as_bool();
            if c["__typename"] == "CheckRun" {
                Check { name: s(&c["name"]), state: ci_of(c["conclusion"].as_str().unwrap_or(""), c["status"].as_str().unwrap_or("")), url: s(&c["detailsUrl"]), required }
            } else {
                Check { name: s(&c["context"]), state: ci_of(c["state"].as_str().unwrap_or(""), ""), url: s(&c["targetUrl"]), required }
            }
        })
        .collect();
    checks.sort_by(|a, b| a.state.cmp(&b.state).then_with(|| a.name.cmp(&b.name)));
    checks
}

/// Overall CI state. Once GitHub said which checks are required, a failing optional check no
/// longer counts as a failure; otherwise GitHub's own rollup state (any failure fails).
fn ci_of_rollup(rollup: &Value, checks: &[Check]) -> Ci {
    let Some(st) = rollup["state"].as_str() else { return Ci::None };
    if !checks.iter().any(|c| c.required.is_some()) {
        return ci_of(st, "");
    }
    checks.iter().filter(|c| !(c.state == Ci::Fail && c.required == Some(false))).map(|c| c.state).min().unwrap_or(Ci::Pass)
}

fn parse_pr(p: &Value, viewer: &str) -> Option<Pr> {
    let number = p["number"].as_u64()?;
    let mut reviewers: Vec<Reviewer> = p["latestReviews"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| { let name = s(&r["author"]["login"]); Reviewer { mine: name == viewer, name, state: Review::parse(r["state"].as_str().unwrap_or("")), at: r["submittedAt"].as_str().and_then(parse_time), re_requested: false, team: false } })
        .collect();
    for rr in p["reviewRequests"]["nodes"].as_array().into_iter().flatten() {
        let who = &rr["requestedReviewer"];
        let team = who["__typename"] == "Team";
        let name = if team { s(&who["name"]) } else { s(&who["login"]) };
        if name.is_empty() {
            continue;
        }
        match reviewers.iter_mut().find(|r| r.name == name) {
            Some(r) => r.re_requested = true,
            None => { let mine = !team && name == viewer; reviewers.push(Reviewer { name, state: Review::Pending, at: None, re_requested: false, team, mine }); }
        }
    }
    let rollup = &p["commits"]["nodes"][0]["commit"]["statusCheckRollup"];
    let checks = parse_checks(rollup);
    let ci = ci_of_rollup(rollup, &checks);
    Some(Pr {
        number,
        repo: s(&p["repository"]["nameWithOwner"]),
        title: s(&p["title"]),
        url: s(&p["url"]),
        draft: p["isDraft"].as_bool().unwrap_or(false),
        head: s(&p["headRefName"]),
        base: s(&p["baseRefName"]),
        additions: p["additions"].as_u64().unwrap_or(0),
        deletions: p["deletions"].as_u64().unwrap_or(0),
        files: p["changedFiles"].as_u64().unwrap_or(0),
        updated: p["updatedAt"].as_str().and_then(parse_time).unwrap_or(0),
        decision: s(&p["reviewDecision"]),
        merge_state: s(&p["mergeStateStatus"]),
        queue: p["mergeQueueEntry"].as_object().map(|q| (q.get("position").and_then(Value::as_u64).unwrap_or(0), q.get("state").and_then(Value::as_str).unwrap_or("").to_string())),
        auto_merge: !p["autoMergeRequest"].is_null(),
        reviewers,
        unresolved: p["reviewThreads"]["nodes"].as_array().map_or(0, |v| v.iter().filter(|t| t["isResolved"] == false).count()),
        ci,
        checks,
    })
}

/// The GraphQL answer → PRs, newest update first.
#[cfg(test)]
pub fn parse(json: &[u8]) -> Result<Vec<Pr>, String> {
    let mut prs = parse_page(json)?.0;
    prs.sort_by_key(|p| std::cmp::Reverse(p.updated));
    Ok(prs)
}

/// One page of the search: its PRs, the cursor of the next page, the total match count.
pub fn parse_page(json: &[u8]) -> Result<(Vec<Pr>, Option<String>, usize), String> {
    let v: Value = serde_json::from_slice(json).map_err(|e| format!("unexpected answer from gh: {e}"))?;
    if let Some(e) = v["errors"][0]["message"].as_str() {
        return Err(e.to_string());
    }
    let search = &v["data"]["search"];
    let viewer = s(&v["data"]["viewer"]["login"]);
    let prs = search["nodes"].as_array().ok_or("unexpected answer from gh")?.iter().filter_map(|p| parse_pr(p, &viewer)).collect();
    let next = (search["pageInfo"]["hasNextPage"] == true).then(|| search["pageInfo"]["endCursor"].as_str().map(str::to_string)).flatten();
    Ok((prs, next, search["issueCount"].as_u64().unwrap_or(0) as usize))
}

/// PRs per request: the per-PR checks and threads make bigger pages time out (HTTP 504).
const PAGE: usize = 20;
/// Most recently updated PRs loaded at most.
pub const MAX_PRS: usize = 60;

/// `git@github.com:o/n.git`, `https://github.com/o/n(.git)` → `o/n`.
pub fn github_slug(remote: &str) -> Option<String> {
    let r = remote.trim().trim_end_matches('/').trim_end_matches(".git");
    let rest = r.strip_prefix("git@github.com:").or_else(|| r.split_once("github.com/").map(|x| x.1))?;
    let mut it = rest.split('/');
    let (o, n) = (it.next()?, it.next()?);
    (!o.is_empty() && !n.is_empty() && it.next().is_none()).then(|| format!("{o}/{n}"))
}

const QUERY: &str = r#"query($q:String!, $n:Int!, $after:String){ viewer{login} search(query:$q, type:ISSUE, first:$n, after:$after){ issueCount pageInfo{ hasNextPage endCursor } nodes{ ... on PullRequest{
 number title url isDraft headRefName baseRefName additions deletions changedFiles updatedAt
 repository{ nameWithOwner }
 reviewDecision mergeStateStatus
 mergeQueueEntry{ position state }
 autoMergeRequest{ enabledAt }
 latestReviews(first:30){ nodes{ state submittedAt author{ login } } }
 reviewRequests(first:30){ nodes{ requestedReviewer{ __typename ... on User{ login } ... on Team{ name } } } }
 reviewThreads(first:100){ nodes{ isResolved } }
 commits(last:1){ nodes{ commit{ statusCheckRollup{ state contexts(first:100){ nodes{ __typename ... on CheckRun{ name status conclusion detailsUrl } ... on StatusContext{ context state targetUrl } } } } } } }
}}}}"#;

/// Fetches the open PRs of the repository at `root` (its `origin` remote must be on GitHub).
/// GitHub search for a scope. Mine: open PRs I wrote in this repository. Review requested: like
/// GitHub's own review list, PRs anywhere in the owner's organisation that ask me personally (not
/// through a team), open, not draft, not archived.
pub fn search_query(slug: &str, scope: Scope) -> String {
    match scope {
        Scope::Mine => format!("repo:{slug} is:pr is:open author:@me sort:updated-desc"),
        Scope::ReviewRequested => {
            let owner = slug.split('/').next().unwrap_or(slug);
            format!("is:pr user-review-requested:@me archived:false is:open user:{owner} draft:false sort:updated-desc")
        }
    }
}

/// Returns the repository slug, the PRs (newest update first, at most [`MAX_PRS`]) and how many
/// match in total.
pub fn fetch(root: &Path, scope: Scope, cancel: &std::sync::atomic::AtomicBool) -> Result<(String, Vec<Pr>, usize), String> {
    let mut g = Command::new("git");
    g.arg("-C").arg(root).args(["remote", "get-url", "origin"]);
    let o = run_bounded(g, Duration::from_secs(5), Some(cancel))?;
    let url = String::from_utf8_lossy(&o.stdout).into_owned();
    let slug = github_slug(&url).ok_or_else(|| format!("origin is not a GitHub repository ({})", url.trim()))?;
    let q = search_query(&slug, scope);
    let (mut prs, mut after) = (Vec::new(), None::<String>);
    let total = loop {
        let mut c = Command::new("gh");
        c.current_dir(root).args(["api", "graphql", "-F"]).arg(format!("q={q}")).args(["-F", &format!("n={PAGE}")]).arg("-f").arg(format!("query={QUERY}"));
        if let Some(a) = &after {
            c.arg("-f").arg(format!("after={a}"));
        }
        let o = run_bounded(c, Duration::from_secs(45), Some(cancel)).map_err(|e| if e.contains("No such file") { "the GitHub CLI `gh` is not installed".to_string() } else { e })?;
        if !o.ok {
            let e = o.stderr.trim();
            return Err(if needs_gh_login(e) {
                format!("GitHub CLI authentication or permissions are missing: {e}")
            } else if e.contains("HTTP 502") || e.contains("HTTP 504") {
                "GitHub timed out answering; try Refresh in a moment".into()
            } else {
                format!("gh failed: {e}")
            });
        }
        let (page, next, n) = parse_page(&o.stdout)?;
        prs.extend(page);
        match next {
            Some(cur) if prs.len() < MAX_PRS => after = Some(cur),
            _ => break n,
        }
    };
    prs.truncate(MAX_PRS);
    resolve_required(root, &mut prs, cancel);
    prs.sort_by_key(|p| std::cmp::Reverse(p.updated));
    Ok((slug, prs, total))
}

/// `isRequired` needs the PR number as an argument, so the search query cannot ask for it. One
/// follow-up query asks it for the PRs with a failing check only (the only ones it can change);
/// when that query fails those PRs keep GitHub's rollup state, where any failure fails.
fn resolve_required(root: &Path, prs: &mut [Pr], cancel: &std::sync::atomic::AtomicBool) {
    let failing: Vec<usize> = (0..prs.len()).filter(|&i| prs[i].ci == Ci::Fail).collect();
    if failing.is_empty() {
        return;
    }
    let Some(q) = required_query(&failing.iter().map(|&i| (prs[i].repo.as_str(), prs[i].number)).collect::<Vec<_>>()) else { return };
    let mut c = Command::new("gh");
    c.current_dir(root).args(["api", "graphql", "-f"]).arg(format!("query={q}"));
    let Ok(o) = run_bounded(c, Duration::from_secs(45), Some(cancel)) else { return };
    let Ok(v) = serde_json::from_slice::<Value>(&o.stdout) else { return };
    for (k, &i) in failing.iter().enumerate() {
        let rollup = &v["data"][format!("p{k}")]["pullRequest"]["commits"]["nodes"][0]["commit"]["statusCheckRollup"];
        apply_required(&mut prs[i], rollup);
    }
}

/// One aliased lookup (`p0`, `p1`, ...) per `(owner/name, number)`; `None` for a malformed slug.
fn required_query(prs: &[(&str, u64)]) -> Option<String> {
    let mut q = String::from("query{");
    for (k, (slug, n)) in prs.iter().enumerate() {
        let (owner, name) = slug.split_once('/')?;
        if [owner, name].iter().any(|x| x.is_empty() || x.contains(['"', '\\'])) {
            return None;
        }
        q += &format!(
            r#" p{k}: repository(owner:"{owner}", name:"{name}"){{ pullRequest(number:{n}){{ commits(last:1){{ nodes{{ commit{{ statusCheckRollup{{ state contexts(first:100){{ nodes{{ __typename ... on CheckRun{{ name status conclusion detailsUrl isRequired(pullRequestNumber:{n}) }} ... on StatusContext{{ context state targetUrl isRequired(pullRequestNumber:{n}) }} }} }} }} }} }} }} }} }}"#
        );
    }
    Some(q + "}")
}

/// Replaces the checks of `pr` with `rollup` from [`required_query`] when it says which are
/// required; leaves `pr` as is otherwise.
fn apply_required(pr: &mut Pr, rollup: &Value) {
    let checks = parse_checks(rollup);
    if checks.is_empty() || checks.iter().all(|c| c.required.is_none()) {
        return;
    }
    pr.ci = ci_of_rollup(rollup, &checks);
    pr.checks = checks;
}

/// Groups PRs into stacks (a PR whose base is another PR's head sits on top of it). Each stack is
/// `(index, depth)` root first; branches fan out depth-first. Single PRs are stacks of one. Stacks
/// are ordered by their most recently updated PR.
pub fn stacks(prs: &[Pr]) -> Vec<Vec<(usize, usize)>> {
    let parent = |i: usize| prs.iter().position(|p| p.head == prs[i].base && p.number != prs[i].number);
    let mut out = Vec::new();
    for root in (0..prs.len()).filter(|&i| parent(i).is_none()) {
        let mut stack: Vec<(usize, usize)> = Vec::new();
        let mut todo = vec![(root, 0)];
        while let Some((i, d)) = todo.pop() {
            if stack.iter().any(|x| x.0 == i) {
                continue;
            }
            stack.push((i, d));
            let mut kids: Vec<usize> = (0..prs.len()).filter(|&k| parent(k) == Some(i)).collect();
            kids.sort_by_key(|&k| std::cmp::Reverse(prs[k].number));
            todo.extend(kids.into_iter().map(|k| (k, d + 1)));
        }
        out.push(stack);
    }
    // a cycle (A on B on A) has no root: keep those PRs visible as singles
    let seen: Vec<usize> = out.iter().flatten().map(|x| x.0).collect();
    out.extend((0..prs.len()).filter(|i| !seen.contains(i)).map(|i| vec![(i, 0)]));
    out.sort_by_key(|s| std::cmp::Reverse(s.iter().map(|&(i, _)| prs[i].updated).max().unwrap_or(0)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"data":{"viewer":{"login":"ann"},"search":{"nodes":[
      {"number":2,"title":"two","url":"u2","isDraft":false,"headRefName":"b2","baseRefName":"b1","additions":3,"deletions":1,"changedFiles":2,"updatedAt":"2026-11-17T19:08:33Z",
       "reviewDecision":"REVIEW_REQUIRED","mergeStateStatus":"BEHIND","mergeQueueEntry":null,"autoMergeRequest":null,
       "latestReviews":{"nodes":[{"state":"APPROVED","submittedAt":"2026-11-17T10:00:00Z","author":{"login":"ann"}},{"state":"CHANGES_REQUESTED","submittedAt":"2026-11-16T10:00:00Z","author":{"login":"bob"}}]},
       "reviewRequests":{"nodes":[{"requestedReviewer":{"__typename":"User","login":"bob"}},{"requestedReviewer":{"__typename":"Team","name":"core"}}]},
       "reviewThreads":{"nodes":[{"isResolved":false},{"isResolved":true}]},
       "commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"FAILURE","contexts":{"nodes":[
          {"__typename":"CheckRun","name":"build","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"c1"},
          {"__typename":"CheckRun","name":"test","status":"COMPLETED","conclusion":"FAILURE","detailsUrl":"c2"},
          {"__typename":"StatusContext","context":"lint","state":"PENDING","targetUrl":"c3"}]}}}}]}},
      {"number":1,"title":"one","url":"u1","isDraft":false,"headRefName":"b1","baseRefName":"main","additions":0,"deletions":0,"changedFiles":0,"updatedAt":"2026-11-10T00:00:00Z",
       "reviewDecision":"APPROVED","mergeStateStatus":"CLEAN","mergeQueueEntry":{"position":3,"state":"QUEUED"},"autoMergeRequest":{"enabledAt":"x"},
       "latestReviews":{"nodes":[]},"reviewRequests":{"nodes":[]},"reviewThreads":{"nodes":[]},"commits":{"nodes":[{"commit":{"statusCheckRollup":null}}]}},
      {}
    ]}}}"#;

    #[test]
    fn graphql_answer_is_parsed_with_reviewers_checks_and_queue() {
        let prs = parse(SAMPLE.as_bytes()).unwrap();
        assert_eq!(prs.iter().map(|p| p.number).collect::<Vec<_>>(), [2, 1], "newest first, empty nodes skipped");
        let p = &prs[0];
        assert_eq!(p.reviewers.len(), 3);
        assert_eq!((p.reviewers[0].name.as_str(), p.reviewers[0].state, p.reviewers[0].re_requested, p.reviewers[0].mine), ("ann", Review::Approved, false, true));
        assert!(p.reviewers[1].re_requested && !p.reviewers[1].mine, "bob was asked again after requesting changes");
        assert!(p.reviewers[2].team && p.reviewers[2].state == Review::Pending && p.reviewers[2].at.is_none());
        assert_eq!(p.checks.iter().map(|c| (c.name.as_str(), c.state)).collect::<Vec<_>>(), [("test", Ci::Fail), ("lint", Ci::Running), ("build", Ci::Pass)], "failures first");
        assert_eq!((p.ci, p.unresolved, p.phase()), (Ci::Fail, 1, Phase::NeedsWork));
        assert_eq!(p.approvals(), 1);
        let q = &prs[1];
        assert_eq!((q.queue.clone(), q.auto_merge, q.ci, q.phase()), (Some((3, "QUEUED".into())), true, Ci::None, Phase::Queued));
        assert!(parse(br#"{"errors":[{"message":"Bad credentials"}]}"#).unwrap_err().contains("Bad credentials"));
    }

    #[test]
    fn github_auth_and_permission_errors_offer_setup() {
        for e in ["gh is not logged in: run gh auth login", "gh failed: HTTP 403: Resource not accessible by integration", "Bad credentials"] {
            assert!(needs_gh_login(e), "{e}");
        }
        assert!(!needs_gh_login("gh failed: HTTP 504"));
    }

    #[test]
    fn review_requested_matches_the_github_review_list_query() {
        assert_eq!(search_query("bearrobotics/pennybot", Scope::ReviewRequested), "is:pr user-review-requested:@me archived:false is:open user:bearrobotics draft:false sort:updated-desc");
        assert_eq!(search_query("o/n", Scope::Mine), "repo:o/n is:pr is:open author:@me sort:updated-desc");
    }

    #[test]
    fn times_and_slugs() {
        assert_eq!(parse_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_time("2026-11-17T19:08:33Z"), Some(1_794_942_513));
        assert_eq!(parse_time("x"), None);
        assert_eq!(github_slug("git@github.com:bearrobotics/pennybot.git").as_deref(), Some("bearrobotics/pennybot"));
        assert_eq!(github_slug("https://github.com/DamChan0/codetrail.git\n").as_deref(), Some("DamChan0/codetrail"));
        assert_eq!(github_slug("https://gitlab.com/a/b"), None);
    }

    fn pr(n: u64, head: &str, base: &str, updated: i64) -> Pr {
        Pr { number: n, repo: String::new(), title: String::new(), url: String::new(), draft: false, head: head.into(), base: base.into(), additions: 0, deletions: 0, files: 0, updated, decision: String::new(), merge_state: String::new(), queue: None, auto_merge: false, reviewers: vec![], unresolved: 0, ci: Ci::None, checks: vec![] }
    }

    #[test]
    fn stacks_chain_by_base_branch_root_first() {
        let prs = vec![pr(3, "c", "b", 5), pr(9, "x", "main", 9), pr(1, "a", "main", 1), pr(2, "b", "a", 2), pr(4, "d", "b", 3), pr(7, "p", "q", 0), pr(8, "q", "p", 0)];
        let st = stacks(&prs);
        let nums: Vec<Vec<u64>> = st.iter().map(|s| s.iter().map(|&(i, _)| prs[i].number).collect()).collect();
        assert_eq!(nums[0], [9], "most recently updated stack first");
        assert_eq!(nums[1], [1, 2, 3, 4], "root, then children depth first (lower number first)");
        assert_eq!(st[1].iter().map(|x| x.1).collect::<Vec<_>>(), [0, 1, 2, 2], "3 and 4 both sit on 2");
        assert!(nums.contains(&vec![7]) && nums.contains(&vec![8]), "a cycle still shows every PR");
    }

    #[test]
    fn phases() {
        let mut p = pr(1, "a", "main", 0);
        assert_eq!(p.phase(), Phase::InReview);
        p.decision = "APPROVED".into();
        assert_eq!(p.phase(), Phase::Ready);
        p.merge_state = "DIRTY".into();
        assert_eq!(p.phase(), Phase::NeedsWork);
        p.draft = true;
        assert_eq!(p.phase(), Phase::Draft);
    }

    /// A rollup as [`required_query`] returns it: `ping` was cancelled, `required` says if the
    /// failing check is required.
    fn rollup_with_failing_ping(required: bool) -> Value {
        serde_json::json!({"state":"FAILURE","contexts":{"nodes":[
            {"__typename":"CheckRun","name":"ping","status":"COMPLETED","conclusion":"CANCELLED","detailsUrl":"","isRequired":required},
            {"__typename":"CheckRun","name":"ping","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"","isRequired":required},
            {"__typename":"StatusContext","context":"coverage-diff","state":"SUCCESS","targetUrl":"","isRequired":true}]}})
    }

    fn approved_failing() -> Pr {
        let mut p = pr(1, "a", "main", 0);
        (p.decision, p.merge_state, p.ci) = ("APPROVED".into(), "BLOCKED".into(), Ci::Fail);
        p
    }

    #[test]
    fn approved_with_only_an_optional_check_failing_is_ready() {
        let mut p = approved_failing();
        assert_eq!(p.phase(), Phase::NeedsWork, "without isRequired any failure fails");
        apply_required(&mut p, &rollup_with_failing_ping(false));
        assert_eq!((p.ci, p.optional_failing(), p.phase()), (Ci::Pass, 1, Phase::Ready));
    }

    #[test]
    fn approved_with_a_required_check_failing_needs_work() {
        let mut p = approved_failing();
        apply_required(&mut p, &rollup_with_failing_ping(true));
        assert_eq!((p.ci, p.optional_failing(), p.phase()), (Ci::Fail, 0, Phase::NeedsWork));
        let mut q = approved_failing();
        apply_required(&mut q, &rollup_with_failing_ping(false));
        q.merge_state = "DIRTY".into();
        assert_eq!(q.phase(), Phase::NeedsWork, "conflicts still need work");
    }

    #[test]
    fn required_query_aliases_each_pr_and_rejects_bad_slugs() {
        let q = required_query(&[("o/a", 7), ("o/b", 9)]).unwrap();
        assert!(q.contains(r#"p0: repository(owner:"o", name:"a"){ pullRequest(number:7)"#) && q.contains("isRequired(pullRequestNumber:9)"));
        assert!(required_query(&[("noslash", 1)]).is_none());
        assert!(required_query(&[(r#"o/a"){x}"#, 1)]).is_none());
    }

    /// Network: `CT_PRS_REPO=/path/to/clone cargo test real_fetch -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_fetch() {
        let root = std::env::var("CT_PRS_REPO").expect("CT_PRS_REPO");
        let scope = if std::env::var("CT_PRS_SCOPE").is_ok_and(|s| s == "review") { Scope::ReviewRequested } else { Scope::Mine };
        let (slug, prs, total) = fetch(Path::new(&root), scope, &std::sync::atomic::AtomicBool::new(false)).unwrap();
        eprintln!("{} of {total}", prs.len());
        for st in stacks(&prs) {
            let line: Vec<String> = st.iter().map(|&(i, _)| format!("#{} {:?} ci={:?} optfail={} rv={}", prs[i].number, prs[i].phase(), prs[i].ci, prs[i].optional_failing(), prs[i].reviewers.len())).collect();
            eprintln!("{slug}: {}", line.join("  ->  "));
        }
        assert!(!prs.is_empty());
    }
}
