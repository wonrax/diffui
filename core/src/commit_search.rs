//! Finding commits by text: what a commit is matched by, how a match scores,
//! and which matches are kept. The palette scores the rows loaded into the
//! graph with these, and the repository actor scores every visible commit
//! ([`crate::repo::Command::SearchCommits`]) with the same ones, so the two
//! rank a commit alike.

use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;

use nucleo_matcher::{Config, Matcher, Utf32Str, Utf32String};

/// The most matches a search keeps. The palette lists no more than this.
pub const MAX_HITS: usize = 200;

/// What a commit is searched by: its ids, description, author and ref
/// labels, written into `out`. `description` is the first line, `None` when
/// the commit has none.
pub fn haystack<'a>(
    change_id: &str,
    commit_id: &str,
    description: Option<&str>,
    author: &str,
    labels: impl IntoIterator<Item = &'a str>,
    out: &mut String,
) {
    out.clear();
    out.push_str(change_id);
    out.push(' ');
    out.push_str(commit_id);
    out.push(' ');
    if let Some(description) = description {
        out.push_str(description);
        out.push(' ');
    }
    out.push_str(author);
    for label in labels {
        out.push(' ');
        out.push_str(label);
    }
}

/// A fuzzy matcher for one needle.
pub struct Scorer {
    matcher: Matcher,
    needle: Utf32String,
    empty: bool,
    chars: Vec<char>,
}

impl Scorer {
    pub fn new(needle: &str) -> Self {
        // nucleo matches case-insensitively by folding the haystack; the
        // needle has to arrive folded already, or an uppercase letter trips
        // its prefilter assertion.
        let needle = needle.to_lowercase();
        Self {
            matcher: Matcher::new(Config::DEFAULT),
            empty: needle.is_empty(),
            needle: Utf32String::from(needle.as_str()),
            chars: Vec::new(),
        }
    }

    /// `haystack`'s score, or `None` when the needle isn't in it. An empty
    /// needle matches everything with a score of zero.
    pub fn score(&mut self, haystack: &str) -> Option<u16> {
        if self.empty {
            return Some(0);
        }
        self.matcher.fuzzy_match(
            Utf32Str::new(haystack, &mut self.chars),
            self.needle.slice(..),
        )
    }
}

/// The best `limit` of a stream of scored items: higher scores first, and the
/// earlier of two equal ones. Holds no more than `limit` at a time, so a broad
/// needle over a long history doesn't keep every match.
pub struct Best<T> {
    limit: usize,
    heap: BinaryHeap<Ranked<T>>,
}

impl<T> Best<T> {
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            heap: BinaryHeap::with_capacity(limit + 1),
        }
    }

    /// Offer the `order`th item. `make` builds it only when it makes the cut.
    pub fn offer(&mut self, score: u16, order: usize, make: impl FnOnce() -> T) {
        if self.limit == 0 {
            return;
        }
        let key = (Reverse(score), order);
        if self.heap.len() == self.limit {
            // The heap's top is the worst kept; a newcomer has to beat it.
            match self.heap.peek() {
                Some(worst) if key < worst.key => {
                    self.heap.pop();
                }
                _ => return,
            }
        }
        self.heap.push(Ranked { key, item: make() });
    }

    /// The kept items, best first, with their scores.
    pub fn into_sorted(self) -> Vec<(u16, T)> {
        let mut ranked = self.heap.into_vec();
        ranked.sort_unstable();
        ranked
            .into_iter()
            .map(|ranked| (ranked.key.0.0, ranked.item))
            .collect()
    }
}

/// An item under its rank. Ordered by the key alone, worst greatest, so the
/// max-heap's top is the one to drop.
struct Ranked<T> {
    key: (Reverse<u16>, usize),
    item: T,
}

impl<T> PartialEq for Ranked<T> {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl<T> Eq for Ranked<T> {}

impl<T> PartialOrd for Ranked<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> Ord for Ranked<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key.cmp(&other.key)
    }
}

/// A commit a history search found, with what a result row shows. Stands on
/// its own: the commit may not be loaded into the graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitHit {
    pub change_id: String,
    pub commit_id: String,
    /// The unique change-id prefix, as the graph would show it.
    pub shortest_change_id_len: Option<usize>,
    /// The `/N` of a divergent change's copy (see
    /// [`crate::CommitSummary::change_offset`]).
    pub change_offset: Option<usize>,
    /// The description's first line; empty when it has none.
    pub description: String,
    pub author: String,
    /// The fuzzy score, as [`Scorer::score`] gave it.
    pub score: u16,
    /// Whether the log revset the search was asked with holds the commit. A
    /// commit outside it never loads into the graph.
    pub in_log: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_best_are_kept_and_ties_go_to_the_earlier() {
        let mut best = Best::new(3);
        for (order, score) in [5u16, 9, 1, 9, 7, 5].into_iter().enumerate() {
            best.offer(score, order, || order);
        }
        assert_eq!(best.into_sorted(), vec![(9, 1), (9, 3), (7, 4)]);
    }

    #[test]
    fn an_uppercase_needle_matches_like_a_lowercase_one() {
        let mut upper = Scorer::new("PARSER");
        let mut lower = Scorer::new("parser");
        assert!(upper.score("fix the parser").is_some());
        assert_eq!(upper.score("fix the parser"), lower.score("fix the parser"));
    }
}
