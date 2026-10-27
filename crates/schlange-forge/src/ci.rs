//! CI as a script: each check has a duration and a conclusion that may depend on the tree it
//! runs on (a marker in a file, two pull requests present together), plus an optional flake
//! rate drawn from a seeded hash, so a run is deterministic for a given tree and attempt.
//!
//! A check can also be a real command (`run`): `sh -c <run>` in a checkout of the tested
//! commit, whose exit code is the result. With real CI on (the `localgit` forge) every check
//! runs as a process: the marker rules are compiled into a `grep` script
//! ([`CiScript::shell_command`]), so the decision is made by a process reading the files of a
//! real checkout. Exit 0: the check's `conclusion`; [`HANG_EXIT`]: the job never reports; any
//! other exit code: failure. `fail_when_prs` names pull requests, which a shell in a checkout
//! cannot see; it stays a rule the forge evaluates on the real tree.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

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
    /// A real command: `sh -c <run>` in a checkout of the tested commit.
    pub run: Option<String>,
}

/// Exit code with which a check command says "this job hangs and never reports" (MQ-37).
pub const HANG_EXIT: i32 = 125;

/// Quotes a string for `sh`.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
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
            run: None,
        }
    }

    /// The command this check runs in a checkout: `run`, or the marker rules compiled into a
    /// `grep` script.
    pub fn shell_command(&self) -> String {
        if let Some(r) = &self.run {
            return r.clone();
        }
        let has = |m: &String| {
            format!(
                "grep -rqF --exclude=.git --exclude-dir=.git -e {} .",
                sh_quote(m)
            )
        };
        let mut s = String::new();
        for m in &self.hang_when_contains {
            s.push_str(&format!("if {}; then exit {HANG_EXIT}; fi\n", has(m)));
        }
        for m in &self.fail_when_contains {
            s.push_str(&format!("if {}; then exit 1; fi\n", has(m)));
        }
        if !self.fail_when_all.is_empty() {
            let all: Vec<String> = self.fail_when_all.iter().map(has).collect();
            s.push_str(&format!("if {}; then exit 1; fi\n", all.join(" && ")));
        }
        s.push_str("exit 0\n");
        s
    }

    /// The conclusion of one real run that exited with `code`. `prs_present`: whether every
    /// pull request of `fail_when_prs` is in the tree; `flake` as in [`CiScript::conclude`].
    pub fn conclude_exit(
        &self,
        code: Option<i32>,
        prs_present: bool,
        flake: f64,
    ) -> Option<Conclusion> {
        let prs_fail = prs_present && !self.fail_when_prs.is_empty();
        match code {
            Some(HANG_EXIT) => None,
            Some(0) if !prs_fail => match self.conclusion {
                Some(Conclusion::Success) if flake < self.flake_rate => Some(Conclusion::Failure),
                c => c,
            },
            _ => Some(Conclusion::Failure),
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

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A fresh directory under the system temporary directory, removed on drop (kept when
/// `SCHLANGE_KEEP_TEMP` is set, for debugging).
#[derive(Debug)]
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(prefix: &str) -> std::io::Result<Self> {
        let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let path =
            std::env::temp_dir().join(format!("{prefix}-{}-{nanos:08x}-{n}", std::process::id()));
        std::fs::create_dir_all(&path)?;
        Ok(TempDir(path))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if std::env::var_os("SCHLANGE_KEEP_TEMP").is_none() {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// Runs `sh -c command` in `dir`; the exit code, `None` when it could not start or was killed.
pub fn run_shell(dir: &Path, command: &str, env: &[(&str, &str)]) -> Option<i32> {
    let mut c = Command::new("sh");
    c.arg("-c")
        .arg(command)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (k, v) in env {
        c.env(k, v);
    }
    c.status().ok().and_then(|s| s.code())
}

/// Writes `tree` into a temporary directory and runs `command` there.
pub fn run_in_tree(tree: &Tree, command: &str, env: &[(&str, &str)]) -> Option<i32> {
    let dir = TempDir::new("schlange-ci").ok()?;
    for (path, content) in tree {
        let file = dir.path().join(path);
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).ok()?;
        }
        std::fs::write(&file, content).ok()?;
    }
    run_shell(dir.path(), command, env)
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
    fn marker_rules_compiled_to_a_shell_script_agree_with_the_scripted_rules() {
        let mut s = CiScript::new("ci", 60, Some(Conclusion::Success));
        s.fail_when_contains = vec!["BUG".into()];
        s.fail_when_all = vec!["SEM_A".into(), "it's B".into()];
        s.hang_when_contains = vec!["HANG".into()];
        let trees: Vec<Tree> = vec![
            [("a".to_string(), "fine\n".to_string())].into(),
            [("d/a".to_string(), "x BUG\n".to_string())].into(),
            [("a".to_string(), "SEM_A\n".to_string())].into(),
            [
                ("a".to_string(), "SEM_A\n".to_string()),
                ("b/c".to_string(), "it's B\n".to_string()),
            ]
            .into(),
            [("a".to_string(), "HANG BUG\n".to_string())].into(),
        ];
        for tree in trees {
            let scripted = s.conclude(&tree, |_| false, 0.5);
            let code = run_in_tree(&tree, &s.shell_command(), &[]);
            assert_eq!(s.conclude_exit(code, false, 0.5), scripted, "{tree:?}");
        }
        let mut r = CiScript::new("ci", 60, Some(Conclusion::Success));
        r.run = Some("test -f ok".into());
        let ok: Tree = [("ok".to_string(), String::new())].into();
        assert_eq!(
            r.conclude_exit(run_in_tree(&ok, &r.shell_command(), &[]), false, 0.5),
            Some(Conclusion::Success)
        );
        assert_eq!(
            r.conclude_exit(
                run_in_tree(&Tree::new(), &r.shell_command(), &[]),
                false,
                0.5
            ),
            Some(Conclusion::Failure)
        );
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
