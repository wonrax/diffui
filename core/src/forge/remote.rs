//! Naming things on a forge: git remote URLs, and the pull-request
//! references people paste.

use super::ForgeKind;

/// A pull (or merge) request: `[host/]owner/name#number` on GitHub,
/// `[host/]group/name!number` on GitLab.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ForgePr {
    pub kind: ForgeKind,
    /// The host, for anything but the forge's public one (a GitHub
    /// Enterprise Server, a self-hosted GitLab). `None` leaves the choice to
    /// the CLI: its host variable when set, the public host otherwise.
    pub host: Option<String>,
    /// The owner or namespace; a GitLab subgroup has slashes in it.
    pub owner: String,
    pub name: String,
    pub number: u64,
}

impl ForgePr {
    /// Parse the forms people paste, for GitHub and GitLab:
    /// - GitHub: a PR URL (`https://github.com/o/r/pull/123`, trailing
    ///   `/files`-style segments and queries tolerated), `o/r#123`,
    ///   `host/o/r#123` and `o/r/pull/123`;
    /// - GitLab: an MR URL (`https://host/group/sub/name/-/merge_requests/7`),
    ///   `group/name!7` and `host/group/name!7`.
    ///
    /// See [`Self::parse_with`].
    pub fn parse(input: &str) -> Option<Self> {
        Self::parse_with(input, &[ForgeKind::GitHub, ForgeKind::GitLab])
    }

    /// Parse a pull request link or reference for one of `kinds`, asked in
    /// order, each by its [`ForgeSpec`](super::ForgeSpec): a URL whose path
    /// reads like its `pull_request_path` (with or without the scheme, or
    /// without the host for a forge with a default host), or
    /// `[host/]owner/name<sigil>number`.
    ///
    /// Returns `None` for anything that doesn't unambiguously name one, so
    /// the open dialog can fall back to treating the input as a local path.
    /// A host without a scheme must look like one (contain a dot) so a
    /// relative path can't be mistaken for it.
    pub fn parse_with(input: &str, kinds: &[ForgeKind]) -> Option<Self> {
        let input = input.trim().trim_end_matches('/');
        // A forge whose default host the input names is asked first, so
        // two forges that write references alike part by host.
        let named = input
            .split_once("://")
            .map_or(input, |(_, rest)| rest)
            .split('/')
            .next()
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        let mut kinds = kinds.to_vec();
        kinds.sort_by_key(|kind| kind.default_host() != named.trim_start_matches("www."));
        let kinds = kinds.as_slice();
        let by_path = |path: &str| kinds.iter().find_map(|kind| Self::parse_path(*kind, path));

        if let Some(rest) = input
            .strip_prefix("https://")
            .or_else(|| input.strip_prefix("http://"))
        {
            let (host, path) = rest.split_once('/')?;
            return by_path(path).map(|pr| pr.on_host(host));
        }

        // `o/r#123`, `group/name!7`, with or without a host in front: the
        // first forge whose sigil is there decides.
        if let Some(kind) = kinds
            .iter()
            .find(|kind| kind.spec().pull_request_path.is_some() && input.contains(kind.sigil()))
        {
            let (slug, number) = input.split_once(kind.sigil())?;
            let number = number.parse().ok()?;
            let parts: Vec<&str> = slug.split('/').collect();
            if parts.iter().any(|part| part.is_empty()) {
                return None;
            }
            let hosted = parts.len() >= 3 && looks_like_host(parts[0]);
            let (host, path) = match (kind.spec().subgroups, parts.len()) {
                (_, _) if hosted && (kind.spec().subgroups || parts.len() == 3) => {
                    (Some(parts[0]), &parts[1..])
                }
                (false, 2) | (true, 2..) => (None, &parts[..]),
                _ => return None,
            };
            if host.is_none() && kind.default_host().is_empty() {
                return None;
            }
            let (name, owner) = path.split_last()?;
            let pr = Self {
                kind: *kind,
                host: None,
                owner: owner.join("/"),
                name: (*name).to_owned(),
                number,
            };
            return Some(match host {
                Some(host) => pr.on_host(host),
                None => pr,
            });
        }

        // `host/o/r/pull/123` (URL form without the scheme).
        if let Some((host, path)) = input.split_once('/')
            && looks_like_host(host)
            && let Some(pr) = by_path(path)
        {
            return Some(pr.on_host(host));
        }

        // Bare `o/r/pull/123` (URL form without the host).
        by_path(input).filter(|pr| !pr.kind.default_host().is_empty())
    }

    /// `kind`'s pull request path (`owner/name/pull/number[...]`), segments
    /// after it tolerated, a query or fragment after the number too.
    fn parse_path(kind: ForgeKind, path: &str) -> Option<Self> {
        let pattern = kind.spec().pull_request_path.as_deref()?;
        let pattern: Vec<&str> = pattern.split('/').collect();
        let segments: Vec<&str> = path.split('/').collect();
        let mut found = Found::default();
        if !match_path(&pattern, &segments, kind.spec().subgroups, &mut found) {
            return None;
        }
        Some(Self {
            kind,
            host: None,
            owner: found.owner?,
            name: found.name?,
            number: found.number?,
        })
    }

    /// `self` on `host`, with the forge's public host folded back to `None`
    /// so the two spellings of one pull request are one tab.
    fn on_host(mut self, host: &str) -> Self {
        let host = host.to_ascii_lowercase();
        let host = host.strip_prefix("www.").unwrap_or(&host);
        self.host = (host != self.kind.default_host() || self.kind.default_host().is_empty())
            .then(|| host.to_owned());
        self
    }

    /// The host the CLI is pointed at, for messages and requests.
    pub fn host_label(&self) -> String {
        match &self.host {
            Some(host) => host.clone(),
            None => self.kind.implied_host(),
        }
    }

    /// Fill in the host from a local repository's remotes: a bare `o/r#123`
    /// typed while a repo whose remote is `git@github.acme.com:o/r.git` is
    /// open means the Enterprise PR, not a github.com one.
    pub fn with_host_from_remotes<'a>(
        self,
        remote_urls: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        if self.host.is_some() {
            return self;
        }
        for url in remote_urls {
            if let Some((host, owner, name)) = parse_remote_url(url)
                && owner.eq_ignore_ascii_case(&self.owner)
                && name.eq_ignore_ascii_case(&self.name)
            {
                return self.on_host(&host);
            }
        }
        self
    }

    /// `owner/name`, the form API paths take.
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// The project, as a [`ForgeRemote`](super::ForgeRemote).
    pub fn remote(&self) -> super::ForgeRemote {
        super::ForgeRemote::new(self.kind, &self.host_label(), &self.owner, &self.name)
    }

    /// Short display label: `name#12`, `name!12`.
    pub fn label(&self) -> String {
        format!("{}{}{}", self.name, self.kind.sigil(), self.number)
    }

    /// The forge's own reference: `owner/name#12`, `group/name!12`.
    pub fn reference(&self) -> String {
        format!("{}{}{}", self.slug(), self.kind.sigil(), self.number)
    }

    /// What identifies it across tabs: the host, the project and the number.
    pub fn key(&self) -> String {
        match &self.host {
            Some(host) => format!("{host}/{}", self.reference()),
            None => self.reference(),
        }
    }
}

/// What a pull request path pattern captured.
#[derive(Default)]
struct Found {
    owner: Option<String>,
    name: Option<String>,
    number: Option<u64>,
}

/// Whether `segments` start the way `pattern` reads, filling in `found`.
/// `{owner}` takes one segment, or several with `subgroups`; `{number}` is the
/// segment's leading digits (`123?diff=split`).
fn match_path(pattern: &[&str], segments: &[&str], subgroups: bool, found: &mut Found) -> bool {
    let Some((first, rest)) = pattern.split_first() else {
        return true;
    };
    match *first {
        "{owner}" => {
            let most = if subgroups {
                segments.len()
            } else {
                1.min(segments.len())
            };
            (1..=most).rev().any(|taken| {
                let owner = &segments[..taken];
                !owner.iter().any(|segment| segment.is_empty())
                    && match_path(rest, &segments[taken..], subgroups, found)
                    && {
                        found.owner = Some(owner.join("/"));
                        true
                    }
            })
        }
        "{name}" => match segments.split_first() {
            Some((name, others)) if !name.is_empty() => {
                found.name = Some((*name).to_owned());
                match_path(rest, others, subgroups, found)
            }
            _ => false,
        },
        "{number}" => match segments.split_first() {
            Some((segment, others)) => {
                let digits = segment
                    .split(|c: char| !c.is_ascii_digit())
                    .next()
                    .unwrap_or("");
                match digits.parse() {
                    Ok(number) => {
                        found.number = Some(number);
                        match_path(rest, others, subgroups, found)
                    }
                    Err(_) => false,
                }
            }
            None => false,
        },
        literal => match segments.split_first() {
            Some((segment, others)) if *segment == literal => {
                match_path(rest, others, subgroups, found)
            }
            _ => false,
        },
    }
}

pub(super) fn looks_like_host(segment: &str) -> bool {
    segment.contains('.') && !segment.starts_with('.') && !segment.contains(['~', ' '])
}

/// `(host, owner, name)` of a git remote URL in any of the forms git takes:
/// `https://host/o/r(.git)`, `ssh://git@host[:port]/o/r`, `git@host:o/r.git`.
/// The owner is everything before the last segment, so a GitLab subgroup
/// (`group/sub`) comes through whole.
pub fn parse_remote_url(url: &str) -> Option<(String, String, String)> {
    let url = url.trim();
    let (host, path) = if let Some((scheme, rest)) = url.split_once("://") {
        if !matches!(scheme, "https" | "http" | "ssh" | "git") {
            return None;
        }
        let (authority, path) = rest.split_once('/')?;
        let host = authority.rsplit('@').next()?;
        let host = host.split(':').next()?;
        (host, path)
    } else {
        // scp-like `[user@]host:path`.
        let (authority, path) = url.split_once(':')?;
        if authority.contains('/') {
            return None;
        }
        (authority.rsplit('@').next()?, path)
    };
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, name) = path.rsplit_once('/')?;
    if host.is_empty() || name.is_empty() || owner.is_empty() || owner.split('/').any(str::is_empty)
    {
        return None;
    }
    Some((host.to_ascii_lowercase(), owner.to_owned(), name.to_owned()))
}

/// The `(name, url)` remotes of the repository at `root` (jj or git), read
/// straight from its git config: best effort, empty when there is none to read.
pub fn remotes(root: &std::path::Path) -> Vec<(String, String)> {
    let git_dir = if root.join(".jj").is_dir() {
        crate::jj::settings::backing_git_dir(root)
    } else {
        crate::repository::resolve_git_dirs(root)
            .ok()
            .map(|dirs| dirs.common_dir)
    };
    let Some(config) = git_dir.and_then(|dir| std::fs::read_to_string(dir.join("config")).ok())
    else {
        return Vec::new();
    };
    remotes_in_config(&config)
}

/// `(name, url)` of the `[remote "…"]` sections of a git config file.
fn remotes_in_config(config: &str) -> Vec<(String, String)> {
    let mut remote: Option<String> = None;
    let mut found = Vec::new();
    for line in config.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            remote = line
                .strip_prefix("[remote \"")
                .and_then(|rest| rest.strip_suffix("\"]"))
                .map(str::to_owned);
            continue;
        }
        let Some(name) = &remote else { continue };
        if let Some((key, value)) = line.split_once('=')
            && key.trim().eq_ignore_ascii_case("url")
        {
            found.push((name.clone(), value.trim().trim_matches('"').to_owned()));
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn github(host: Option<&str>, owner: &str, name: &str, number: u64) -> Option<ForgePr> {
        Some(ForgePr {
            kind: ForgeKind::GitHub,
            host: host.map(str::to_owned),
            owner: owner.to_owned(),
            name: name.to_owned(),
            number,
        })
    }

    fn gitlab(host: Option<&str>, owner: &str, name: &str, number: u64) -> Option<ForgePr> {
        Some(ForgePr {
            kind: ForgeKind::GitLab,
            ..github(host, owner, name, number)?
        })
    }

    #[test]
    fn parses_pr_urls() {
        let expected = github(None, "anthropics", "claude-code", 123);
        for input in [
            "https://github.com/anthropics/claude-code/pull/123",
            "https://github.com/anthropics/claude-code/pull/123/files?diff=split",
            "github.com/anthropics/claude-code/pull/123",
            "anthropics/claude-code/pull/123",
            "anthropics/claude-code#123",
        ] {
            assert_eq!(ForgePr::parse(input), expected, "{input}");
        }
    }

    #[test]
    fn parses_enterprise_hosts() {
        let expected = github(Some("github.acme.com"), "team", "app", 42);
        assert_eq!(
            ForgePr::parse("https://github.acme.com/team/app/pull/42/files"),
            expected
        );
        assert_eq!(ForgePr::parse("github.acme.com/team/app/pull/42"), expected);
        assert_eq!(ForgePr::parse("github.acme.com/team/app#42"), expected);
        assert_eq!(ForgePr::parse("GitHub.Acme.com/team/app#42"), expected);
        // github.com spelled out is the same PR as the shorthand.
        assert_eq!(
            ForgePr::parse("https://www.github.com/team/app/pull/42"),
            ForgePr::parse("team/app#42")
        );
        // A relative path with three segments isn't a host reference.
        assert_eq!(ForgePr::parse("code/team/app#42"), None);
    }

    #[test]
    fn parses_merge_requests() {
        assert_eq!(
            ForgePr::parse("https://gitlab.com/group/sub/app/-/merge_requests/7/diffs"),
            gitlab(None, "group/sub", "app", 7)
        );
        assert_eq!(
            ForgePr::parse("https://git.acme.org/infra/tools/-/merge_requests/12"),
            gitlab(Some("git.acme.org"), "infra", "tools", 12)
        );
        assert_eq!(
            ForgePr::parse("group/app!7"),
            gitlab(None, "group", "app", 7)
        );
        assert_eq!(
            ForgePr::parse("group/sub/app!7"),
            gitlab(None, "group/sub", "app", 7)
        );
        assert_eq!(
            ForgePr::parse("git.acme.org/infra/tools!3"),
            gitlab(Some("git.acme.org"), "infra", "tools", 3)
        );
        assert_eq!(
            ForgePr::parse("gitlab.com/group/app!7"),
            gitlab(None, "group", "app", 7)
        );
        assert_eq!(ForgePr::parse("app!7"), None);
        let mr = gitlab(None, "group/sub", "app", 7).expect("an MR");
        assert_eq!(mr.label(), "app!7");
        assert_eq!(mr.reference(), "group/sub/app!7");
    }

    #[test]
    fn a_plugins_forge_reads_its_own_links() {
        let forgejo = ForgeKind::Script(crate::forge::ScriptForge::new(crate::forge::ForgeSpec {
            id: "forgejo".into(),
            name: "Forgejo".into(),
            sigil: '#',
            short: "PR".into(),
            pull_request_path: Some("{owner}/{name}/pulls/{number}".into()),
            ..Default::default()
        }));
        let kinds = [ForgeKind::GitHub, forgejo];
        let pr = ForgePr::parse_with("https://codeberg.org/team/app/pulls/12/files", &kinds)
            .expect("a pull request");
        assert_eq!(pr.kind, forgejo);
        assert_eq!(pr.host.as_deref(), Some("codeberg.org"));
        assert_eq!(
            (pr.owner.as_str(), pr.name.as_str(), pr.number),
            ("team", "app", 12)
        );
        assert_eq!(pr.host_label(), "codeberg.org");
        // A host-less reference needs a forge with a default host: GitHub's.
        assert_eq!(
            ForgePr::parse_with("team/app#3", &kinds).unwrap().kind,
            ForgeKind::GitHub
        );
        let alone = [forgejo];
        assert_eq!(ForgePr::parse_with("team/app#3", &alone), None);
        assert_eq!(
            ForgePr::parse_with("codeberg.org/team/app#3", &alone).map(|pr| pr.key()),
            Some("codeberg.org/team/app#3".to_owned())
        );
        assert_eq!(ForgePr::parse_with("team/app/pulls/3", &alone), None);
    }

    #[test]
    fn rejects_non_pr_inputs() {
        // Local paths must fall through to the repository-open flow.
        for input in [
            "~/code/diffui",
            "/abs/path/repo",
            "relative/repo",
            "a/b/c#1",
            "owner/repo#notanumber",
            "https://github.com/anthropics/claude-code/issues/123",
            "",
        ] {
            assert_eq!(ForgePr::parse(input), None, "{input}");
        }
    }

    #[test]
    fn parses_remote_urls() {
        let expected = Some((
            "github.acme.com".to_owned(),
            "team".to_owned(),
            "app".to_owned(),
        ));
        assert_eq!(
            parse_remote_url("git@github.acme.com:team/app.git"),
            expected
        );
        assert_eq!(
            parse_remote_url("https://github.acme.com/team/app"),
            expected
        );
        assert_eq!(
            parse_remote_url("ssh://git@github.acme.com:2222/team/app.git"),
            expected
        );
        assert_eq!(
            parse_remote_url("git@gitlab.com:group/sub/app.git"),
            Some((
                "gitlab.com".to_owned(),
                "group/sub".to_owned(),
                "app".to_owned()
            ))
        );
        assert_eq!(
            parse_remote_url("https://gitlab.com/group/sub/deeper/app/"),
            Some((
                "gitlab.com".to_owned(),
                "group/sub/deeper".to_owned(),
                "app".to_owned()
            ))
        );
        assert_eq!(parse_remote_url("/srv/git/app.git"), None);
        assert_eq!(parse_remote_url("file:///srv/git/team/app.git"), None);
        assert_eq!(parse_remote_url("git@host:app.git"), None);
    }

    #[test]
    fn shorthand_takes_the_host_of_a_matching_remote() {
        let config = r#"
[core]
	bare = false
[remote "origin"]
	url = git@github.acme.com:team/app.git
	fetch = +refs/heads/*:refs/remotes/origin/*
[remote "fork"]
	url = https://github.com/me/app.git
"#;
        let remotes = remotes_in_config(config);
        assert_eq!(
            remotes,
            vec![
                (
                    "origin".to_owned(),
                    "git@github.acme.com:team/app.git".to_owned()
                ),
                (
                    "fork".to_owned(),
                    "https://github.com/me/app.git".to_owned()
                ),
            ]
        );
        let urls = || remotes.iter().map(|(_, url)| url.as_str());
        let pr = ForgePr::parse("team/app#42")
            .expect("a reference")
            .with_host_from_remotes(urls());
        assert_eq!(Some(pr), github(Some("github.acme.com"), "team", "app", 42));
        let pr = ForgePr::parse("me/app#1")
            .expect("a reference")
            .with_host_from_remotes(urls());
        assert_eq!(pr.host, None, "github.com stays the default");
    }
}
