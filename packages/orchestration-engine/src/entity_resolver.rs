#![deny(unsafe_code)]
//! Connected-account entity resolution: map "my X on github" prompts to the
//! account's own repository URLs. Entities come from the connected account
//! directory at runtime — never constructed from a guessed username, and
//! never hardcoded. Ambiguity fails closed to `None` instead of guessing
//! between two matches.

use crate::intent_resolver::{NON_IDENTIFYING, PLURAL_MARKERS, content_tokens, tokens};

/// One repository known to the connected account, as reported by the
/// directory (API-sourced `html_url`, never string-built).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoRef {
    pub owner: String,
    pub name: String,
    pub html_url: String,
}

/// Account directory failures. Resolution maps every error to "no entity"
/// and falls through to the next resolver tier — a broken directory degrades
/// to table routes, never to a guess.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DirectoryError {
    #[error("account directory unavailable")]
    Unavailable,
    #[error("account directory response invalid")]
    InvalidResponse,
}

/// Connected accounts and their repositories. Synchronous by design: adapters
/// that need I/O must bound it internally (cache out-of-band, timeout fast),
/// because resolution runs on the dispatch path.
///
/// No production adapter exists yet — there is no stored GitHub credential
/// anywhere in the workspace to call the API with, and inventing token
/// plumbing would be a security change, not a routing one. The tier stays
/// inert (`None`) until an adapter is wired with explicit user consent.
pub trait AccountDirectory: Send + Sync {
    /// Repositories visible to the connected account.
    ///
    /// # Errors
    /// Returns [`DirectoryError`] when the directory cannot answer.
    fn github_repos(&self) -> Result<Vec<RepoRef>, DirectoryError>;
}

/// Words that can never name a repo entity: portal markers, self-reference,
/// collection words, and the existing non-identifying vocabulary.
fn is_entity_noise(token: &str) -> bool {
    matches!(token, "github" | "gh" | "my" | "mine")
        || PLURAL_MARKERS.contains(&token)
        || NON_IDENTIFYING.contains(&token)
}

/// Resolve a repo-entity prompt (`my X on github`) against the connected
/// account's repos. Requires an explicit self-reference (`my`/`mine`) plus
/// a `github`/`gh` marker; the candidate is the last remaining content
/// token, matched case-insensitively as a substring in either direction.
/// Zero or several matches both yield `None` — ambiguity never guesses.
#[must_use]
pub fn resolve_repo_entity(prompt: &str, dir: &dyn AccountDirectory) -> Option<String> {
    let raw: Vec<String> = tokens(prompt);
    let marks = |word: &str| raw.iter().any(|token| token == word);
    if !marks("github") && !marks("gh") {
        return None;
    }
    if !marks("my") && !marks("mine") {
        return None;
    }
    let candidate = content_tokens(prompt)
        .into_iter()
        .rev()
        .find(|token| !is_entity_noise(token.as_str()))?;
    let repos = dir.github_repos().ok()?;
    let mut hits = repos.iter().filter(|repo| {
        let name = repo.name.to_ascii_lowercase();
        name.contains(candidate.as_str()) || candidate.contains(name.as_str())
    });
    let hit = hits.next()?;
    if hits.next().is_some() {
        return None;
    }
    Some(hit.html_url.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(repos: Vec<RepoRef>) -> FixtureDirectory {
        FixtureDirectory { repos }
    }

    fn repo(owner: &str, name: &str) -> RepoRef {
        RepoRef {
            owner: owner.into(),
            name: name.into(),
            html_url: format!("https://github.com/{owner}/{name}"),
        }
    }

    struct FixtureDirectory {
        repos: Vec<RepoRef>,
    }

    impl AccountDirectory for FixtureDirectory {
        fn github_repos(&self) -> Result<Vec<RepoRef>, DirectoryError> {
            Ok(self.repos.clone())
        }
    }

    #[test]
    fn entity_resolver_matches_connected_repo() {
        let dir = fixture(vec![
            repo("fixture-owner", "portopsy"),
            repo("fixture-owner", "website"),
        ]);
        assert_eq!(
            resolve_repo_entity("check out my portopsy on github", &dir).as_deref(),
            Some("https://github.com/fixture-owner/portopsy")
        );
    }

    #[test]
    fn entity_resolver_returns_none_on_ambiguity() {
        let dir = fixture(vec![
            repo("fixture-owner", "portopsy"),
            repo("fixture-owner", "portopsy-fork"),
        ]);
        assert_eq!(
            resolve_repo_entity("check out my portopsy on github", &dir),
            None
        );
    }
}
