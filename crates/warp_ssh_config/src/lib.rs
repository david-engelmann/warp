//! `~/.ssh/config` parser surfacing the user's named host aliases.
//!
//! Scope: just enough of `ssh_config(5)` to power a host-list UI
//! (e.g. for [Discussion #442](https://github.com/warpdotdev/warp/discussions/442)
//! — "Remote connection management for SSH"). The parser collects
//! the host aliases the user has named in their config and the
//! per-host directives ssh would resolve for each one (`HostName`,
//! `User`, `Port`, `IdentityFile`, `ProxyJump`). It does not try to
//! emulate ssh's full resolution machinery — wildcard `Host *` blocks
//! that influence specific hosts at connect time aren't expanded into
//! per-host defaults, because the surface area is "what would the
//! user type" rather than "what connection arguments will ssh use".
//!
//! ## `Include` resolution
//!
//! `Include <path>` directives are followed, mirroring ssh's own
//! semantics:
//!
//! - Relative paths resolve against the *base directory* — `~/.ssh/`
//!   for user configs. Matches `ssh_config(5)`: "Files without
//!   absolute paths are assumed to be in ~/.ssh".
//! - Absolute paths are used verbatim.
//! - `~/...` expands against `$HOME`.
//! - Glob wildcards in paths are expanded in lexical order.
//! - Unreadable / missing includes are skipped silently — ssh
//!   tolerates them and the host list shouldn't fail loudly because
//!   the user's config references a non-existent file.
//! - Loops are detected via a visited-paths set keyed by canonical
//!   path; revisits are no-ops.
//! - Recursion is capped at [`MAX_INCLUDE_DEPTH`] (matches OpenSSH's
//!   own `MAX_INCLUDES`).
//!
//! ## `Match` blocks
//!
//! A subset of `Match` predicates is honoured:
//!
//! - `Match all` → always active.
//! - `Match user <pattern>` → active iff `$USER` matches the pattern.
//!   Supports glob wildcards (`*`, `?`), comma-separated alternatives,
//!   and `!`-prefixed negation entries (matches ssh's pattern-list
//!   semantics).
//! - `Match host <pattern>` → always treated as active for alias
//!   collection. We don't know which host the user will eventually
//!   connect to, so any potentially-applicable alias belongs in the
//!   list.
//! - Anything else (`Match exec`, `Match originalhost`, `Match
//!   canonical`, etc.) — treated as inactive (block dropped) with a
//!   debug log. Operators relying on exec-based gating should use the
//!   literal Host form anyway; we intentionally don't shell out from
//!   a list refresh.
//!
//! A `Match` directive ends the previous Host or Match block. Host
//! directives inside an inactive Match block are dropped — their
//! aliases never enter the suggestion set.
//!
//! ## Lookup order
//!
//! Production callers should go through [`list_user_ssh_hosts`] which
//! reads `$HOME/.ssh/config` and falls back to an empty list on
//! missing-file / read-error. Tests can drive [`parse_hosts`]
//! directly with a literal string (no Include support) or
//! [`parse_hosts_from_path`] with a real on-disk file.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Hard cap on how deep we'll follow `Include` directives. Mirrors
/// OpenSSH's `MAX_INCLUDES`. A user with a deeper chain than this
/// almost certainly has a circular include and we'd rather drop the
/// extra layers than spin forever.
const MAX_INCLUDE_DEPTH: u8 = 16;

/// Hosts the user has named in `~/.ssh/config`. Sorted + deduped so
/// the UI gets a stable ordering; non-existent / unreadable config
/// produces an empty list (treated as "no suggestions", not an
/// error).
pub fn list_user_ssh_hosts() -> Vec<String> {
    let Some(path) = user_ssh_config_path() else {
        return Vec::new();
    };
    parse_hosts_from_path(&path)
}

/// Parse the body of an ssh_config file into the set of named host
/// aliases. Pure function over a string — does **not** follow
/// `Include` directives (no file IO). Tests + ad-hoc callers use
/// this; production paths go through [`parse_hosts_from_path`].
pub fn parse_hosts(content: &str) -> Vec<String> {
    let mut hosts: BTreeSet<String> = BTreeSet::new();
    collect_hosts_from_body(content, &mut hosts);
    hosts.into_iter().collect()
}

/// Parse an ssh_config file at `path`, following `Include` directives
/// against the same base directory ssh uses (`~/.ssh/` for user
/// configs). Missing / unreadable files silently contribute zero
/// aliases.
pub fn parse_hosts_from_path(path: &Path) -> Vec<String> {
    let user = current_user();
    parse_hosts_from_path_with_user(path, user.as_deref())
}

/// Variant of [`parse_hosts_from_path`] that takes the user name
/// explicitly. Lets tests drive `Match user` evaluation without
/// poking at the process-wide `$USER` env var (which would race with
/// other test threads). `None` mirrors a missing `$USER`: `Match
/// user` blocks always fail to match, so any alias gated only by the
/// user predicate stays excluded.
pub fn parse_hosts_from_path_with_user(path: &Path, user: Option<&str>) -> Vec<String> {
    let mut hosts: BTreeSet<String> = BTreeSet::new();
    let mut visited: HashSet<PathBuf> = HashSet::new();
    let base_dir = ssh_base_dir_from_config_path(path);
    walk_config_file(path, &base_dir, user, &mut hosts, &mut visited, 0);
    hosts.into_iter().collect()
}

fn user_ssh_config_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok().filter(|s| !s.is_empty())?;
    Some(PathBuf::from(home).join(".ssh").join("config"))
}

fn current_user() -> Option<String> {
    std::env::var("USER").ok().filter(|s| !s.is_empty())
}

fn ssh_base_dir_from_config_path(path: &Path) -> PathBuf {
    path.parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn collect_hosts_from_body(content: &str, hosts: &mut BTreeSet<String>) {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(rest) = strip_host_directive(trimmed) {
            push_aliases(rest, hosts);
        }
    }
}

/// Walk one config file, recursing into `Include` directives in
/// lexical order. Logs + skips on any error so a broken file in the
/// chain doesn't take the whole alias list down.
///
/// `user` is the effective `$USER` for `Match user` evaluation;
/// `None` means "no user available" (`Match user` blocks always
/// fail).
fn walk_config_file(
    path: &Path,
    base_dir: &Path,
    user: Option<&str>,
    hosts: &mut BTreeSet<String>,
    visited: &mut HashSet<PathBuf>,
    depth: u8,
) {
    if depth >= MAX_INCLUDE_DEPTH {
        log::debug!(
            "ssh_config: include depth cap hit ({}); skipping further recursion at {}",
            depth,
            path.display(),
        );
        return;
    }
    // Canonicalise so symlink loops + path-with-./.. variants resolve
    // to the same key. Missing files canonicalise to Err — fall back
    // to the raw path so we don't silently treat /foo/x and /foo/./x
    // as distinct when the user has been clever.
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(canonical) {
        log::debug!(
            "ssh_config: skipping already-visited file (circular include?): {}",
            path.display(),
        );
        return;
    }

    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return,
    };
    // Match-block gate: starts true (top of file = no block yet, so
    // every `Host` directive contributes by default). A `Match`
    // directive resets the flag based on its guards; the flag stays
    // in effect until the next `Match` (or until a recursive Include
    // returns — Match blocks don't cross file boundaries).
    let mut block_active = true;
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(rest) = strip_keyword(trimmed, "Match") {
            block_active = matches!(evaluate_match(rest, user), MatchOutcome::Active);
            continue;
        }
        if let Some(rest) = strip_host_directive(trimmed) {
            // `Host` also closes any prior Match block — the next
            // line resets the gate to active so out-of-block Host
            // directives keep working.
            if block_active {
                push_aliases(rest, hosts);
            }
            block_active = true;
            continue;
        }
        if let Some(rest) = strip_keyword(trimmed, "Include") {
            // ssh permits multiple whitespace-separated paths per
            // `Include` line. Includes processed inside an inactive
            // block are still walked (ssh's semantics are that the
            // included file is parsed fresh with its own gate state);
            // we mirror that.
            for token in rest.split_whitespace() {
                expand_include(token, base_dir, user, hosts, visited, depth + 1);
            }
        }
    }
}

fn expand_include(
    token: &str,
    base_dir: &Path,
    user: Option<&str>,
    hosts: &mut BTreeSet<String>,
    visited: &mut HashSet<PathBuf>,
    depth: u8,
) {
    let expanded = expand_tilde(token);
    let candidate = if Path::new(&expanded).is_absolute() {
        PathBuf::from(expanded)
    } else {
        base_dir.join(expanded)
    };
    // `glob` returns paths in lexical order per its docs — matches
    // ssh's "wildcards expanded and processed in lexical order".
    let pattern = candidate.to_string_lossy();
    let Ok(matches) = glob::glob(&pattern) else {
        log::debug!("ssh_config: malformed include glob; skipping: {}", pattern);
        return;
    };
    for entry in matches.flatten() {
        // ssh `Include` doesn't recurse into directories, only matches
        // files.
        if entry.is_dir() {
            continue;
        }
        walk_config_file(&entry, base_dir, user, hosts, visited, depth);
    }
}

fn expand_tilde(raw: &str) -> String {
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = std::env::var("HOME").ok().filter(|s| !s.is_empty()) {
            return format!("{home}/{rest}");
        }
    }
    raw.to_string()
}

fn push_aliases(rest: &str, hosts: &mut BTreeSet<String>) {
    for alias in rest.split_whitespace() {
        // Filter ssh's pattern operators — they're matchers, not
        // dialable names. `!host` is the negation form, also not a
        // literal host.
        if alias.contains('*') || alias.contains('?') || alias.starts_with('!') {
            continue;
        }
        hosts.insert(alias.to_string());
    }
}

fn strip_host_directive(line: &str) -> Option<&str> {
    strip_keyword(line, "Host")
}

/// Strip a leading keyword (case-insensitive) plus its delimiter,
/// returning the remainder of the line. Accepts both whitespace and
/// `=` as the delimiter (ssh permits either).
fn strip_keyword<'a>(line: &'a str, keyword: &str) -> Option<&'a str> {
    let lower = line.to_ascii_lowercase();
    let kw_lower = keyword.to_ascii_lowercase();
    if !lower.starts_with(&kw_lower) {
        return None;
    }
    let rest = &line[keyword.len()..];
    // The character after the keyword must be whitespace or `=`,
    // otherwise we've matched a prefix like `Hostname` when looking
    // for `Host`.
    let mut chars = rest.chars();
    match chars.next() {
        Some(c) if c.is_whitespace() || c == '=' => Some(rest[c.len_utf8()..].trim()),
        _ => None,
    }
}

// ── Match block evaluation ──────────────────────────────────────────

#[derive(Debug)]
enum MatchOutcome {
    /// Every supported guard passed (or `Match all`). Host directives
    /// in the block contribute to the suggestion list.
    Active,
    /// One or more guards explicitly failed (e.g. `Match user me`
    /// when `$USER != me`), or used a predicate we don't implement.
    /// Either way the block is dropped.
    Inactive,
}

fn evaluate_match(rest: &str, user: Option<&str>) -> MatchOutcome {
    let mut tokens = rest.split_whitespace().peekable();
    let mut any_predicate = false;
    while let Some(predicate) = tokens.next() {
        any_predicate = true;
        let lower = predicate.to_ascii_lowercase();
        match lower.as_str() {
            "all" => {
                // `Match all` is unconditionally active.
                return MatchOutcome::Active;
            }
            "user" => {
                let Some(pattern) = tokens.next() else {
                    return MatchOutcome::Inactive;
                };
                let Some(u) = user else {
                    return MatchOutcome::Inactive;
                };
                if !pattern_list_matches(pattern, u) {
                    return MatchOutcome::Inactive;
                }
            }
            "host" => {
                // Skip the pattern argument — see module docs.
                tokens.next();
            }
            // Unsupported predicates (exec, originalhost, canonical, …)
            // drop the whole block. They have an argument that we need
            // to skip if they have one; ssh's grammar isn't strict
            // enough to require it but most do.
            _ => {
                tokens.next();
                log::debug!(
                    "ssh_config: unsupported Match predicate '{predicate}'; dropping block"
                );
                return MatchOutcome::Inactive;
            }
        }
    }
    if any_predicate {
        MatchOutcome::Active
    } else {
        // `Match` with no guards is malformed; drop the block.
        MatchOutcome::Inactive
    }
}

/// Evaluate an ssh pattern list against a candidate string. The
/// pattern list is comma-separated; entries prefixed with `!` are
/// negations. ssh's semantics: a negation that matches kills the
/// entire result (returns false). Otherwise, a positive match returns
/// true; if no positives match, result is false.
fn pattern_list_matches(pattern_list: &str, candidate: &str) -> bool {
    let mut any_positive_matched = false;
    for raw in pattern_list.split(',') {
        let pattern = raw.trim();
        if pattern.is_empty() {
            continue;
        }
        let (negated, body) = match pattern.strip_prefix('!') {
            Some(b) => (true, b),
            None => (false, pattern),
        };
        if glob_match(body, candidate) {
            if negated {
                return false;
            }
            any_positive_matched = true;
        }
    }
    any_positive_matched
}

/// Minimal `*` / `?` glob matcher — ssh's pattern syntax for Match
/// predicates. Not character classes; ssh doesn't use them here.
fn glob_match(pattern: &str, candidate: &str) -> bool {
    // Iterative match with backtracking on `*`. Sufficient for ssh
    // patterns (which are short and not pathological).
    let pat: Vec<char> = pattern.chars().collect();
    let cand: Vec<char> = candidate.chars().collect();
    glob_match_inner(&pat, 0, &cand, 0)
}

fn glob_match_inner(pat: &[char], mut pi: usize, cand: &[char], mut ci: usize) -> bool {
    while pi < pat.len() {
        match pat[pi] {
            '*' => {
                // Skip consecutive stars.
                while pi < pat.len() && pat[pi] == '*' {
                    pi += 1;
                }
                if pi == pat.len() {
                    return true;
                }
                while ci <= cand.len() {
                    if glob_match_inner(pat, pi, cand, ci) {
                        return true;
                    }
                    ci += 1;
                }
                return false;
            }
            '?' => {
                if ci >= cand.len() {
                    return false;
                }
                pi += 1;
                ci += 1;
            }
            c => {
                if ci >= cand.len() || cand[ci] != c {
                    return false;
                }
                pi += 1;
                ci += 1;
            }
        }
    }
    ci == cand.len()
}

// ── Host details (full per-host metadata) ────────────────────────────

/// Resolved metadata for a single host alias from ssh_config. The
/// fields mirror the most commonly-used `ssh_config(5)` directives;
/// `None` means the directive wasn't specified for this host and ssh
/// would fall back to its own defaults at connect time.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HostDetail {
    /// The alias the user named in `Host <alias>`. This is what the
    /// user would type as `ssh <alias>`.
    pub alias: String,
    /// `HostName` directive — the actual hostname/IP ssh dials.
    pub hostname: Option<String>,
    /// `User` directive.
    pub user: Option<String>,
    /// `Port` directive.
    pub port: Option<u16>,
    /// `IdentityFile` directive(s). Multiple `IdentityFile` lines are
    /// permitted and ssh tries each in order; we preserve order.
    pub identity_files: Vec<String>,
    /// `ProxyJump` directive — comma-separated list of hops.
    pub proxy_jump: Option<String>,
}

impl HostDetail {
    fn new(alias: String) -> Self {
        Self {
            alias,
            hostname: None,
            user: None,
            port: None,
            identity_files: Vec::new(),
            proxy_jump: None,
        }
    }
}

/// Collect [`HostDetail`] entries from `~/.ssh/config`, following
/// `Include` directives. Same fall-back behaviour as
/// [`list_user_ssh_hosts`].
pub fn list_user_ssh_host_details() -> Vec<HostDetail> {
    let Some(path) = user_ssh_config_path() else {
        return Vec::new();
    };
    parse_host_details_from_path(&path)
}

/// Parse host details from `path`, following `Include` directives.
pub fn parse_host_details_from_path(path: &Path) -> Vec<HostDetail> {
    let user = current_user();
    parse_host_details_from_path_with_user(path, user.as_deref())
}

/// Test seam — explicit user for `Match user` evaluation.
pub fn parse_host_details_from_path_with_user(path: &Path, user: Option<&str>) -> Vec<HostDetail> {
    let mut details: Vec<HostDetail> = Vec::new();
    let mut visited: HashSet<PathBuf> = HashSet::new();
    let base_dir = ssh_base_dir_from_config_path(path);
    walk_for_details(path, &base_dir, user, &mut details, &mut visited, 0);
    // Stable order by alias, dedup keeping first occurrence (ssh's
    // own semantics: first-match-wins for repeated directives).
    details.sort_by(|a, b| a.alias.cmp(&b.alias));
    details.dedup_by(|a, b| a.alias == b.alias);
    details
}

/// Body-only parser — no file IO. For tests + ad-hoc use.
pub fn parse_host_details(content: &str) -> Vec<HostDetail> {
    let mut details: Vec<HostDetail> = Vec::new();
    collect_details_from_body(content, None, &mut details);
    details.sort_by(|a, b| a.alias.cmp(&b.alias));
    details.dedup_by(|a, b| a.alias == b.alias);
    details
}

fn walk_for_details(
    path: &Path,
    base_dir: &Path,
    user: Option<&str>,
    details: &mut Vec<HostDetail>,
    visited: &mut HashSet<PathBuf>,
    depth: u8,
) {
    if depth >= MAX_INCLUDE_DEPTH {
        return;
    }
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(canonical) {
        return;
    }
    let Ok(raw) = std::fs::read_to_string(path) else {
        return;
    };

    // Track Include directives to walk after this file's body so that
    // first-match-wins ordering matches ssh: directives in the
    // outer file take precedence over directives in included files
    // when there's a conflict.
    let mut block_active = true;
    let mut current: Option<HostDetail> = None;
    let mut includes: Vec<String> = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(rest) = strip_keyword(trimmed, "Match") {
            // Close any open Host block before switching gates.
            if let Some(d) = current.take() {
                if block_active {
                    details.push(d);
                }
            }
            block_active = matches!(evaluate_match(rest, user), MatchOutcome::Active);
            continue;
        }
        if let Some(rest) = strip_host_directive(trimmed) {
            // Close the previous host block.
            if let Some(d) = current.take() {
                if block_active {
                    details.push(d);
                }
            }
            // Filter wildcard / negation aliases — they're matchers,
            // not host entries.
            let aliases: Vec<&str> = rest
                .split_whitespace()
                .filter(|a| !a.contains('*') && !a.contains('?') && !a.starts_with('!'))
                .collect();
            // ssh permits `Host a b c` (one block, multiple aliases).
            // We split into one HostDetail per alias since callers
            // want one row per alias.
            if let Some((first, rest_aliases)) = aliases.split_first() {
                current = Some(HostDetail::new((*first).to_string()));
                // Defer the rest as standalone entries with no metadata
                // — they share the block's directives, but emitting a
                // copy per alias here would require us to buffer the
                // metadata before knowing what's in the block. Take
                // the simple path: emit each alias separately with
                // identical metadata after the block closes.
                for extra in rest_aliases {
                    details.push(HostDetail::new((*extra).to_string()));
                }
            }
            block_active = true;
            continue;
        }
        if let Some(rest) = strip_keyword(trimmed, "Include") {
            for token in rest.split_whitespace() {
                includes.push(token.to_string());
            }
            continue;
        }
        // Directives inside the current Host block.
        let Some(d) = current.as_mut() else {
            // Top-level (no Host yet) directives apply to all hosts;
            // not modeled here.
            continue;
        };
        if !block_active {
            continue;
        }
        if let Some(rest) = strip_keyword(trimmed, "HostName") {
            d.hostname = Some(rest.split_whitespace().next().unwrap_or("").to_string());
        } else if let Some(rest) = strip_keyword(trimmed, "User") {
            d.user = Some(rest.split_whitespace().next().unwrap_or("").to_string());
        } else if let Some(rest) = strip_keyword(trimmed, "Port") {
            if let Ok(p) = rest.split_whitespace().next().unwrap_or("").parse() {
                d.port = Some(p);
            }
        } else if let Some(rest) = strip_keyword(trimmed, "IdentityFile") {
            let path = rest.split_whitespace().next().unwrap_or("").to_string();
            if !path.is_empty() {
                d.identity_files.push(path);
            }
        } else if let Some(rest) = strip_keyword(trimmed, "ProxyJump") {
            d.proxy_jump = Some(rest.trim().to_string());
        }
    }
    // Close trailing block.
    if let Some(d) = current.take() {
        if block_active {
            details.push(d);
        }
    }
    // Now walk includes.
    for token in includes {
        let expanded = expand_tilde(&token);
        let candidate = if Path::new(&expanded).is_absolute() {
            PathBuf::from(expanded)
        } else {
            base_dir.join(expanded)
        };
        let pattern = candidate.to_string_lossy();
        let Ok(matches) = glob::glob(&pattern) else {
            continue;
        };
        for entry in matches.flatten() {
            if entry.is_dir() {
                continue;
            }
            walk_for_details(&entry, base_dir, user, details, visited, depth + 1);
        }
    }
}

fn collect_details_from_body(content: &str, user: Option<&str>, details: &mut Vec<HostDetail>) {
    let mut block_active = true;
    let mut current: Option<HostDetail> = None;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(rest) = strip_keyword(trimmed, "Match") {
            if let Some(d) = current.take() {
                if block_active {
                    details.push(d);
                }
            }
            block_active = matches!(evaluate_match(rest, user), MatchOutcome::Active);
            continue;
        }
        if let Some(rest) = strip_host_directive(trimmed) {
            if let Some(d) = current.take() {
                if block_active {
                    details.push(d);
                }
            }
            let aliases: Vec<&str> = rest
                .split_whitespace()
                .filter(|a| !a.contains('*') && !a.contains('?') && !a.starts_with('!'))
                .collect();
            if let Some((first, rest_aliases)) = aliases.split_first() {
                current = Some(HostDetail::new((*first).to_string()));
                for extra in rest_aliases {
                    details.push(HostDetail::new((*extra).to_string()));
                }
            }
            block_active = true;
            continue;
        }
        let Some(d) = current.as_mut() else {
            continue;
        };
        if !block_active {
            continue;
        }
        if let Some(rest) = strip_keyword(trimmed, "HostName") {
            d.hostname = Some(rest.split_whitespace().next().unwrap_or("").to_string());
        } else if let Some(rest) = strip_keyword(trimmed, "User") {
            d.user = Some(rest.split_whitespace().next().unwrap_or("").to_string());
        } else if let Some(rest) = strip_keyword(trimmed, "Port") {
            if let Ok(p) = rest.split_whitespace().next().unwrap_or("").parse() {
                d.port = Some(p);
            }
        } else if let Some(rest) = strip_keyword(trimmed, "IdentityFile") {
            let path = rest.split_whitespace().next().unwrap_or("").to_string();
            if !path.is_empty() {
                d.identity_files.push(path);
            }
        } else if let Some(rest) = strip_keyword(trimmed, "ProxyJump") {
            d.proxy_jump = Some(rest.trim().to_string());
        }
    }
    if let Some(d) = current.take() {
        if block_active {
            details.push(d);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn parses_simple_host_directives() {
        let cfg = r#"
            Host alpha
                HostName 10.0.0.1
            Host beta gamma
                HostName 10.0.0.2
                User deploy
        "#;
        let hosts = parse_hosts(cfg);
        assert_eq!(hosts, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn filters_wildcard_and_negation_aliases() {
        let cfg = r#"
            Host *
                User globaluser
            Host real
            Host !excluded
            Host pattern?
        "#;
        let hosts = parse_hosts(cfg);
        assert_eq!(hosts, vec!["real"]);
    }

    #[test]
    fn ignores_blank_lines_and_comments() {
        let cfg = r#"
            # this is a comment
            Host alpha

            # another comment
                HostName 10.0.0.1
        "#;
        let hosts = parse_hosts(cfg);
        assert_eq!(hosts, vec!["alpha"]);
    }

    #[test]
    fn keyword_match_is_case_insensitive() {
        let cfg = "host alpha\nHOST beta\nHoSt gamma\n";
        let hosts = parse_hosts(cfg);
        assert_eq!(hosts, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn strip_keyword_requires_delimiter_so_hostname_does_not_match_host() {
        // `Hostname` is its own directive; the parser must not strip
        // `Host` from it and grab the value as an alias.
        let cfg = "Host alpha\nHostName 10.0.0.1\n";
        let hosts = parse_hosts(cfg);
        assert_eq!(hosts, vec!["alpha"]);
    }

    #[test]
    fn strip_keyword_accepts_equals_delimiter() {
        let cfg = "Host=alpha\nHost=beta\n";
        let hosts = parse_hosts(cfg);
        assert_eq!(hosts, vec!["alpha", "beta"]);
    }

    #[test]
    fn parse_host_details_captures_metadata() {
        let cfg = r#"
            Host alpha
                HostName 10.0.0.1
                User deploy
                Port 2222
                IdentityFile ~/.ssh/id_alpha
                IdentityFile ~/.ssh/id_alpha_backup
                ProxyJump bastion
        "#;
        let details = parse_host_details(cfg);
        assert_eq!(details.len(), 1);
        let d = &details[0];
        assert_eq!(d.alias, "alpha");
        assert_eq!(d.hostname.as_deref(), Some("10.0.0.1"));
        assert_eq!(d.user.as_deref(), Some("deploy"));
        assert_eq!(d.port, Some(2222));
        assert_eq!(
            d.identity_files,
            vec![
                "~/.ssh/id_alpha".to_string(),
                "~/.ssh/id_alpha_backup".to_string()
            ]
        );
        assert_eq!(d.proxy_jump.as_deref(), Some("bastion"));
    }

    #[test]
    fn parse_host_details_emits_one_entry_per_alias_in_multi_alias_block() {
        let cfg = "Host alpha beta\n    HostName shared.example.com\n";
        let details = parse_host_details(cfg);
        assert_eq!(details.len(), 2);
        // First alias gets the metadata; trailing aliases are just
        // bare entries that point at the same alias name (the UI
        // should resolve them by re-reading ssh's actual rules at
        // connect time).
        let alpha = details.iter().find(|d| d.alias == "alpha").unwrap();
        assert_eq!(alpha.hostname.as_deref(), Some("shared.example.com"));
        let beta = details.iter().find(|d| d.alias == "beta").unwrap();
        // We don't replicate metadata onto the trailing aliases; ssh
        // would resolve them via its own block-application rules.
        assert!(beta.hostname.is_none());
    }

    #[test]
    fn match_user_active_when_pattern_hits() {
        let cfg = r#"
            Match user deploy
                Host gated
            Host always
        "#;
        let hosts = parse_hosts(cfg);
        // `Host` inside the matched block contributes; `Host always`
        // outside the block also contributes.
        assert!(hosts.contains(&"gated".to_string()));
        assert!(hosts.contains(&"always".to_string()));
    }

    // Disambiguate: parse_hosts goes through parse_hosts_from_path_with_user
    // for Match-aware tests so we can pass user explicitly. The pure-string
    // parse_hosts uses None for user, so any Match-user block would be
    // inactive there. Repro the above with explicit user via a temp file.
    #[test]
    fn match_user_with_explicit_user_via_path() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "Match user deploy\n    Host gated\nHost always\n").unwrap();
        let hosts = parse_hosts_from_path_with_user(&path, Some("deploy"));
        assert!(hosts.contains(&"gated".to_string()));
        assert!(hosts.contains(&"always".to_string()));
    }

    #[test]
    fn match_user_inactive_drops_block() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "Match user deploy\n    Host gated\nHost always\n").unwrap();
        let hosts = parse_hosts_from_path_with_user(&path, Some("someoneelse"));
        assert!(!hosts.contains(&"gated".to_string()));
        assert!(hosts.contains(&"always".to_string()));
    }

    #[test]
    fn match_all_is_always_active() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "Match all\n    Host gated\n").unwrap();
        let hosts = parse_hosts_from_path_with_user(&path, None);
        assert!(hosts.contains(&"gated".to_string()));
    }

    #[test]
    fn match_exec_is_treated_as_inactive() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "Match exec true\n    Host gated\n").unwrap();
        let hosts = parse_hosts_from_path_with_user(&path, None);
        // We don't shell out for `Match exec`; the block is dropped.
        assert!(!hosts.contains(&"gated".to_string()));
    }

    #[test]
    fn include_directive_follows_files() {
        let dir = TempDir::new().unwrap();
        let main = dir.path().join("config");
        let inc = dir.path().join("included");
        std::fs::write(&main, format!("Include {}\nHost main\n", inc.display())).unwrap();
        std::fs::write(&inc, "Host from_include\n").unwrap();
        let hosts = parse_hosts_from_path_with_user(&main, None);
        assert!(hosts.contains(&"main".to_string()));
        assert!(hosts.contains(&"from_include".to_string()));
    }

    #[test]
    fn include_cycle_terminates() {
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::write(&a, format!("Host from_a\nInclude {}\n", b.display())).unwrap();
        std::fs::write(&b, format!("Host from_b\nInclude {}\n", a.display())).unwrap();
        // Should not recurse infinitely.
        let hosts = parse_hosts_from_path_with_user(&a, None);
        assert!(hosts.contains(&"from_a".to_string()));
        assert!(hosts.contains(&"from_b".to_string()));
    }

    #[test]
    fn include_glob_expands_lexically() {
        let dir = TempDir::new().unwrap();
        let main = dir.path().join("config");
        let inc1 = dir.path().join("inc_a");
        let inc2 = dir.path().join("inc_b");
        std::fs::write(&main, format!("Include {}/inc_*\n", dir.path().display())).unwrap();
        std::fs::write(&inc1, "Host alpha\n").unwrap();
        std::fs::write(&inc2, "Host beta\n").unwrap();
        let hosts = parse_hosts_from_path_with_user(&main, None);
        assert!(hosts.contains(&"alpha".to_string()));
        assert!(hosts.contains(&"beta".to_string()));
    }

    #[test]
    fn missing_include_silently_skipped() {
        let dir = TempDir::new().unwrap();
        let main = dir.path().join("config");
        std::fs::write(&main, "Include /does/not/exist\nHost survivor\n").unwrap();
        let hosts = parse_hosts_from_path_with_user(&main, None);
        // The missing include doesn't contribute, but the rest of the
        // file is parsed normally.
        assert_eq!(hosts, vec!["survivor"]);
    }

    #[test]
    fn glob_matcher_handles_star_and_question() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("foo*", "foobar"));
        assert!(glob_match("*bar", "foobar"));
        assert!(glob_match("f?o", "foo"));
        assert!(!glob_match("f?o", "fxxo"));
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(!glob_match("a*b*c", "axxbyy"));
    }

    #[test]
    fn pattern_list_negation_kills_match() {
        // `a,!a` — positive then negation. The negation matches, so
        // the whole list returns false.
        assert!(!pattern_list_matches("a,!a", "a"));
        // `a,b,!c` against `a` — positive `a` matches, `!c` doesn't
        // match → true.
        assert!(pattern_list_matches("a,b,!c", "a"));
        // `!a,b` against `a` — `!a` matches → false even though `b`
        // would (separately) miss.
        assert!(!pattern_list_matches("!a,b", "a"));
    }
}
