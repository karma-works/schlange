//! A small DSL to write scenarios in Rust tests, in the spirit of rust-lang/bors' test harness:
//!
//! ```
//! use schlange_forge::dsl::{Sim, ci};
//! let report = Sim::new("demo", "queue_rules:\n  - name: default\n    queue_conditions:\n      - check-success = ci\npull_request_rules:\n  - name: q\n    conditions: []\n    actions:\n      queue:\n")
//!     .check(ci("ci", "2m"))
//!     .pr(1, &[("a.txt", "a\n")])
//!     .pr(2, &[("b.txt", "b\n")])
//!     .open("0s", 1)
//!     .open("10s", 2)
//!     .run();
//! report.assert_invariants();
//! assert_eq!(report.outcome(1), "merged");
//! assert_eq!(report.merged_order, vec![1, 2]);
//! ```

use std::collections::BTreeMap;

use schlange_config::rules::{Rules, parse_duration};
use schlange_core::model::{Conclusion, Time};

use crate::ci::{CiScript, RunsOn};
use crate::runner::{RunReport, Scenario, run};
use crate::scenario::default_base_files;
use crate::sim::{Action, Faults, PrDef};

/// A check script: `ci("ci-ok", "3m").queue_only().fails_on("BUG")`.
pub fn ci(check: &str, duration: &str) -> CiScript {
    CiScript::new(check, secs(duration), Some(Conclusion::Success))
}

fn secs(d: &str) -> Time {
    parse_duration(d).unwrap_or_else(|| panic!("bad duration `{d}`"))
}

impl CiScript {
    pub fn queue_only(mut self) -> Self {
        self.runs_on = RunsOn::Queue;
        self
    }
    pub fn pr_only(mut self) -> Self {
        self.runs_on = RunsOn::PullRequest;
        self
    }
    pub fn fails_on(mut self, marker: &str) -> Self {
        self.fail_when_contains.push(marker.to_string());
        self
    }
    pub fn fails_with_all(mut self, markers: &[&str]) -> Self {
        self.fail_when_all = markers.iter().map(|m| m.to_string()).collect();
        self
    }
    pub fn fails_with_prs(mut self, prs: &[u64]) -> Self {
        self.fail_when_prs = prs.to_vec();
        self
    }
    pub fn flaky(mut self, rate: f64) -> Self {
        self.flake_rate = rate;
        self
    }
    pub fn never(mut self) -> Self {
        self.conclusion = None;
        self
    }
}

pub struct Sim {
    s: Scenario,
    overrides: BTreeMap<u64, BTreeMap<String, Option<Conclusion>>>,
}

impl Sim {
    /// A scenario on `main` with the given `.mergify.yml` text.
    pub fn new(name: &str, config: &str) -> Self {
        let rules = Rules::from_yaml(config).unwrap_or_else(|e| panic!("config: {e}"));
        Sim {
            s: Scenario {
                name: name.to_string(),
                description: String::new(),
                config: "(inline)".into(),
                rules,
                base: "main".into(),
                base_files: default_base_files(),
                queue_scripts: vec![],
                prs: vec![],
                steps: vec![],
                expect: vec![],
                expect_merged_order: None,
                expect_base: BTreeMap::new(),
                expect_log: vec![],
                faults: Some(Faults::standard(1)),
                flake_seed: 0,
                max_time: 7 * 86400,
            },
            overrides: BTreeMap::new(),
        }
    }

    pub fn check(mut self, script: CiScript) -> Self {
        self.s.queue_scripts.push(script);
        self
    }

    pub fn base_file(mut self, path: &str, content: &str) -> Self {
        self.s.base_files.insert(path.into(), content.into());
        self
    }

    /// A pull request by `bot` on `main` changing these files.
    pub fn pr(self, n: u64, files: &[(&str, &str)]) -> Self {
        self.pr_with(n, files, |_| {})
    }

    pub fn pr_with(mut self, n: u64, files: &[(&str, &str)], f: impl FnOnce(&mut PrDef)) -> Self {
        let mut def = PrDef {
            number: n,
            author: "bot".into(),
            title: format!("Pull request {n}"),
            base: "main".into(),
            draft: false,
            labels: Default::default(),
            files: files
                .iter()
                .map(|(p, c)| (p.to_string(), c.to_string()))
                .collect(),
            dependabot_update_types: vec![],
            scripts: vec![],
        };
        f(&mut def);
        self.s.prs.push(def);
        self
    }

    /// Overrides a check's conclusion on the pull request itself (not on queue drafts).
    pub fn pr_check(mut self, n: u64, check: &str, conclusion: Option<Conclusion>) -> Self {
        self.overrides
            .entry(n)
            .or_default()
            .insert(check.into(), conclusion);
        self
    }

    pub fn at(mut self, t: &str, action: Action) -> Self {
        self.s.steps.push((secs(t), action));
        self
    }

    pub fn open(self, t: &str, n: u64) -> Self {
        self.at(t, Action::Open(n))
    }

    pub fn label(self, t: &str, n: u64, label: &str) -> Self {
        self.at(
            t,
            Action::Label {
                number: n,
                label: label.into(),
            },
        )
    }

    pub fn push(self, t: &str, n: u64, files: &[(&str, &str)]) -> Self {
        self.at(
            t,
            Action::Push {
                number: n,
                files: files
                    .iter()
                    .map(|(p, c)| (p.to_string(), c.to_string()))
                    .collect(),
            },
        )
    }

    pub fn push_base(self, t: &str, files: &[(&str, &str)]) -> Self {
        self.at(
            t,
            Action::PushBase {
                branch: "main".into(),
                files: files
                    .iter()
                    .map(|(p, c)| (p.to_string(), c.to_string()))
                    .collect(),
            },
        )
    }

    pub fn faults(mut self, faults: Option<Faults>) -> Self {
        self.s.faults = faults;
        self
    }

    pub fn flake_seed(mut self, seed: u64) -> Self {
        self.s.flake_seed = seed;
        self
    }

    /// The scenario as built, with per-PR scripts resolved.
    pub fn build(mut self) -> Scenario {
        for p in &mut self.s.prs {
            p.scripts = self
                .s
                .queue_scripts
                .iter()
                .map(|s| {
                    let mut s = s.clone();
                    if let Some(c) = self.overrides.get(&p.number).and_then(|o| o.get(&s.check)) {
                        s.conclusion = *c;
                    }
                    s
                })
                .collect();
        }
        self.s
    }

    pub fn run(self) -> RunReport {
        run(&self.build())
    }
}

impl RunReport {
    /// Panics with the full report when an invariant is violated.
    pub fn assert_invariants(&self) {
        if self.invariants.iter().any(|i| i.status == "violated") {
            panic!("invariant violated\n{}", self.to_text());
        }
    }

    /// `merged`, `dequeued CODE`, `queued`, `open` or `closed`.
    pub fn outcome(&self, n: u64) -> &str {
        self.outcomes
            .get(&n)
            .map(String::as_str)
            .unwrap_or("unknown")
    }
}
