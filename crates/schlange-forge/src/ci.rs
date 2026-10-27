//! CI as a script: each check has a duration and a conclusion that may depend on the tree it
//! runs on (a marker in a file, two pull requests present together), plus an optional flake
//! rate drawn from a seeded hash, so a run is deterministic for a given tree and attempt.

use schlange_core::model::{Conclusion, Time};

use crate::git::Tree;

/// Where a check runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RunsOn {
    /// On pull requests and on queue draft pull requests.
    #[default]
    Both,
    /// Only on pull requests that are not the queue's.
    PullRequest,
    /// Only on queue draft pull requests (`mergify/merge-queue/*` heads).
    Queue,
}

#[derive(Clone, Debug)]
pub struct CiScript {
    pub check: String,
    pub app: Option<String>,
    pub duration: Time,
    /// Conclusion when no rule below applies; `None`: the check never reports.
    pub conclusion: Option<Conclusion>,
    pub runs_on: RunsOn,
    /// Fails when any file contains one of these markers.
    pub fail_when_contains: Vec<String>,
    /// Fails when the tree contains all of these markers (semantic conflict, MQ-01).
    pub fail_when_all: Vec<String>,
    /// Fails when all of these pull requests' changes are in the tree (semantic conflict).
    pub fail_when_prs: Vec<u64>,
    /// Never reports when any file contains one of these markers (a hanging job, MQ-37).
    pub hang_when_contains: Vec<String>,
    /// Probability that a run which would succeed fails anyway.
    pub flake_rate: f64,
}

impl CiScript {
    pub fn new(check: &str, duration: Time, conclusion: Option<Conclusion>) -> Self {
        CiScript {
            check: check.to_string(),
            app: None,
            duration,
            conclusion,
            runs_on: RunsOn::Both,
            fail_when_contains: vec![],
            fail_when_all: vec![],
            fail_when_prs: vec![],
            hang_when_contains: vec![],
            flake_rate: 0.0,
        }
    }

    pub fn runs_on_queue(&self) -> bool {
        self.runs_on != RunsOn::PullRequest
    }

    pub fn runs_on_pull_request(&self) -> bool {
        self.runs_on != RunsOn::Queue
    }

    /// The conclusion of one run on `tree`. `present(n)` says whether pull request `n`'s
    /// changes are in the tree; `flake` is a uniform draw in [0, 1) for this run.
    pub fn conclude(
        &self,
        tree: &Tree,
        present: impl Fn(u64) -> bool,
        flake: f64,
    ) -> Option<Conclusion> {
        let contains = |m: &String| tree.values().any(|c| c.contains(m.as_str()));
        if self.hang_when_contains.iter().any(contains) {
            return None;
        }
        if self.fail_when_contains.iter().any(contains)
            || (!self.fail_when_all.is_empty() && self.fail_when_all.iter().all(contains))
            || (!self.fail_when_prs.is_empty() && self.fail_when_prs.iter().all(|n| present(*n)))
        {
            return Some(Conclusion::Failure);
        }
        match self.conclusion {
            Some(Conclusion::Success) if flake < self.flake_rate => Some(Conclusion::Failure),
            c => c,
        }
    }
}

/// SplitMix64: a small deterministic generator for fault injection and flakes.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        mix(self.0)
    }

    /// Uniform in [0, 1).
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform in [0, n].
    pub fn upto(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % (n + 1) }
    }
}

fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A uniform draw in [0, 1) keyed by the seed and a text (tree id, check, attempt): the same
/// key gives the same draw, whatever the order of runs.
pub fn keyed_unit(seed: u64, key: &str) -> f64 {
    let mut h = seed;
    for b in key.bytes() {
        h = mix(h ^ u64::from(b));
    }
    (mix(h) >> 11) as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_rules() {
        let tree: Tree = [("a".to_string(), "uses foo".to_string())].into();
        let mut s = CiScript::new("ci", 60, Some(Conclusion::Success));
        assert_eq!(s.conclude(&tree, |_| false, 0.5), Some(Conclusion::Success));
        s.fail_when_all = vec!["uses foo".into(), "renamed foo".into()];
        assert_eq!(s.conclude(&tree, |_| false, 0.5), Some(Conclusion::Success));
        s.fail_when_contains = vec!["foo".into()];
        assert_eq!(s.conclude(&tree, |_| false, 0.5), Some(Conclusion::Failure));
        let mut p = CiScript::new("ci", 60, Some(Conclusion::Success));
        p.fail_when_prs = vec![1, 2];
        assert_eq!(
            p.conclude(&tree, |n| n == 1, 0.5),
            Some(Conclusion::Success)
        );
        assert_eq!(p.conclude(&tree, |_| true, 0.5), Some(Conclusion::Failure));
        p.flake_rate = 0.3;
        assert_eq!(p.conclude(&tree, |_| false, 0.2), Some(Conclusion::Failure));
        assert_eq!(p.conclude(&tree, |_| false, 0.4), Some(Conclusion::Success));
    }

    #[test]
    fn keyed_draws_are_stable() {
        assert_eq!(keyed_unit(1, "t/ci/0"), keyed_unit(1, "t/ci/0"));
        assert_ne!(keyed_unit(1, "t/ci/0"), keyed_unit(1, "t/ci/1"));
        let mut r = Rng::new(7);
        let a: Vec<u64> = (0..3).map(|_| r.upto(10)).collect();
        let mut r = Rng::new(7);
        let b: Vec<u64> = (0..3).map(|_| r.upto(10)).collect();
        assert_eq!(a, b);
    }
}
