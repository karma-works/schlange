//! Invariants I0–I5 (KONZEPT.md, section 6), checked by the scenario runner after every step
//! with the full knowledge of the simulated forge. The checker never asks the engine whether it
//! did the right thing: it judges from the forge's records (cars built, landings, commits).
//!
//! - **I0** Every commit on a base branch is the initial commit, a commit made outside the
//!   queue, or a queue landing whose tree equals the tree the queue built and tested for exactly
//!   that prefix, on top of exactly the base it assumed; the car's checks were green. With
//!   batches and `squash`, the commits inside a batch are prefixes of the tested branch whose
//!   checks ran only on the batch's tip (Mergify's semantics too); for batch size 1 the check
//!   is strict per commit.
//! - **I1** No pull request lands whose queue or merge conditions were false at landing time
//!   (on the forge's truth) or whose head is not the tested head.
//! - **I2** At quiescence every queued pull request was merged or dequeued with a reason; a
//!   queue that halted on purpose (tree mismatch) is reported with its reason.
//! - **I3** After every step, the queue branches on the forge (prefix plus ten hex characters)
//!   and the open draft pull requests are exactly those of the engine's cars.
//! - **I4** At no step more cars (running speculative checks) than `max_parallel_checks`.
//! - **I5** Same end state (merged pull requests, base trees, pull requests left queued) with
//!   perfect and with disturbed webhook delivery; compared by the runner.
//!
//! The checker reads commits, trees, parents and refs through the forge's [`GitStore`], so on
//! the `localgit` forge it judges the real repository on disk: the first-parent history of the
//! base comes from `git rev-list`, the tree of every landed commit from `git rev-parse`.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use schlange_config::rules::Rules;
use schlange_core::State;
use schlange_core::eval::{self, Context};
use schlange_core::model::PullRequest;

use crate::git::{GitStore, Sha};
use crate::sim::SimForge;

#[derive(Clone, Debug, Default, Serialize)]
pub struct InvariantState {
    /// How many individual checks ran.
    pub checks: u64,
    pub violations: Vec<String>,
    pub notes: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Checker {
    pub results: BTreeMap<&'static str, InvariantState>,
    landings_seen: usize,
    accounted: BTreeSet<Sha>,
    landed_commits: BTreeSet<Sha>,
}

pub const IDS: [&str; 6] = ["I0", "I1", "I2", "I3", "I4", "I5"];

fn is_queue_branch(name: &str, prefixes: &[String]) -> bool {
    prefixes.iter().any(|p| {
        name.strip_prefix(p.as_str()).is_some_and(|rest| {
            rest.len() == 10 && rest.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
        })
    })
}

impl Checker {
    pub fn new() -> Self {
        let mut c = Checker::default();
        for id in IDS {
            c.results.insert(id, InvariantState::default());
        }
        c
    }

    fn check(&mut self, id: &'static str, ok: bool, t: u64, msg: impl FnOnce() -> String) {
        let r = self.results.get_mut(id).expect("known id");
        r.checks += 1;
        if !ok {
            r.violations
                .push(format!("{}: {}", crate::runner::clock(t), msg()));
        }
    }

    pub fn note(&mut self, id: &'static str, msg: String) {
        self.results.get_mut(id).expect("known id").notes.push(msg);
    }

    pub fn violated(&self) -> bool {
        self.results.values().any(|r| !r.violations.is_empty())
    }

    /// Runs after every simulated step, once the engine's decisions were carried out.
    pub fn after_step<G: GitStore>(&mut self, forge: &SimForge<G>, state: &State, rules: &Rules) {
        let t = forge.now();
        self.landings(forge, rules, t);
        self.base_history(forge, t);
        self.branches(forge, state, rules, t);
        self.parallel(forge, state, rules, t);
    }

    fn landings<G: GitStore>(&mut self, forge: &SimForge<G>, rules: &Rules, t: u64) {
        let new: Vec<_> = forge.landings[self.landings_seen..].to_vec();
        self.landings_seen = forge.landings.len();
        for l in new {
            if l.commits.is_empty() {
                continue;
            }
            for c in &l.commits {
                self.landed_commits.insert(c.sha.clone());
            }
            let Some(car) = forge.cars.get(&l.car) else {
                self.check("I0", false, t, || {
                    format!("landing of car {} that the forge never built", l.car)
                });
                continue;
            };
            // I1: heads and conditions on the truth right before landing.
            let rule = rules.queue_rules.iter().find(|q| q.name == l.queue);
            let merge_conds = rule.map(|r| {
                if r.merge_conditions.is_empty() {
                    r.queue_conditions.clone()
                } else {
                    r.merge_conditions.clone()
                }
            });
            let mut green = true;
            for c in &l.commits {
                let before = l.before.iter().find(|p| p.number == c.number);
                let requested = l.requested.iter().find(|p| p.number == c.number);
                let tested = car.prefix.iter().find(|(n, ..)| *n == c.number);
                let heads_ok = match (before, requested) {
                    (Some(b), Some(r)) => {
                        b.head_sha == r.head_sha
                            && (car.in_place || tested.is_some())
                            && pr_head_in_car(forge, car, b)
                    }
                    _ => false,
                };
                self.check("I1", heads_ok, t, || {
                    format!("#{} landed with a head that was not tested", c.number)
                });
                let (Some(rule), Some(conds), Some(before)) = (rule, &merge_conds, before) else {
                    self.check("I1", false, t, || {
                        format!("#{} landed without a queue rule or truth", c.number)
                    });
                    green = false;
                    continue;
                };
                let ctx = Context::default();
                let (q_ok, _) = eval::eval_list(&rule.queue_conditions, before, &ctx, &mut vec![]);
                self.check("I1", q_ok, t, || {
                    format!("#{} landed while its queue_conditions were false", c.number)
                });
                let view = merge_view(before, l.draft_before.as_ref(), &car.head);
                let (m_ok, _) = eval::eval_list(conds, &view, &ctx, &mut vec![]);
                self.check("I1", m_ok, t, || {
                    format!("#{} landed while its merge conditions were false", c.number)
                });
                green &= m_ok;
            }
            // I0: every landed commit is the tested prefix, on the assumed base.
            let first = l.commits[0].number;
            let Some(idx0) = car.prefix.iter().position(|(n, ..)| *n == first) else {
                self.check("I0", false, t, || format!("#{first} is not in its car"));
                continue;
            };
            let parent = forge.repo.parents(&l.commits[0].sha).first().cloned();
            let expected_parent_tree = if idx0 == 0 {
                forge.repo.tree_id_of(&car.base_sha)
            } else {
                car.prefix[idx0 - 1].2.clone()
            };
            let parent_ok =
                parent.is_some_and(|p| forge.repo.tree_id_of(&p) == expected_parent_tree);
            self.check("I0", parent_ok, t, || {
                format!(
                    "#{first} landed on a base whose tree is not the one car {} was tested on",
                    car.id
                )
            });
            for (k, c) in l.commits.iter().enumerate() {
                // The tree as the repository has it, not as the forge's reply claims.
                let real_tree = forge.repo.tree_id_of(&c.sha);
                let ok = real_tree == c.tree
                    && car
                        .prefix
                        .get(idx0 + k)
                        .is_some_and(|(n, _, tree)| *n == c.number && *tree == real_tree);
                self.check("I0", ok, t, || {
                    format!(
                        "commit {} of #{} has a tree that car {} did not build",
                        &c.sha[..7],
                        c.number,
                        car.id
                    )
                });
            }
            self.check("I0", green, t, || {
                format!("car {} landed without green merge checks", car.id)
            });
        }
    }

    fn base_history<G: GitStore>(&mut self, forge: &SimForge<G>, t: u64) {
        for base in &forge.base_branches {
            let Some(tip) = forge.read_branch(base) else {
                continue;
            };
            for sha in forge.repo.first_parent_history(&tip) {
                if self.accounted.contains(&sha) {
                    break;
                }
                let ok = sha == forge.initial_commit
                    || forge.external_commits.contains(&sha)
                    || self.landed_commits.contains(&sha);
                self.check("I0", ok, t, || {
                    format!("commit {} on {base} came from nowhere", &sha[..7])
                });
                self.accounted.insert(sha);
            }
        }
    }

    fn branches<G: GitStore>(&mut self, forge: &SimForge<G>, state: &State, rules: &Rules, t: u64) {
        let prefixes: Vec<String> = rules
            .queue_rules
            .iter()
            .map(|q| q.queue_branch_prefix.clone())
            .collect();
        let on_forge: BTreeSet<String> = forge
            .repo
            .ref_names()
            .into_iter()
            .filter(|r| is_queue_branch(r, &prefixes))
            .collect();
        let in_use: BTreeSet<String> = state.trains.values().flat_map(|t| t.branches()).collect();
        self.check("I3", on_forge == in_use, t, || {
            format!("queue branches on the forge {on_forge:?} ≠ branches in use {in_use:?}")
        });
        let drafts: BTreeSet<u64> = forge.open_drafts().iter().map(|p| p.number).collect();
        let car_drafts: BTreeSet<u64> = state
            .trains
            .values()
            .flat_map(|t| {
                t.cars
                    .iter()
                    .filter(|c| !c.in_place)
                    .filter_map(|c| c.draft)
            })
            .collect();
        self.check("I3", drafts == car_drafts, t, || {
            format!("open draft pull requests {drafts:?} ≠ drafts of cars {car_drafts:?}")
        });
    }

    fn parallel<G: GitStore>(&mut self, forge: &SimForge<G>, state: &State, rules: &Rules, t: u64) {
        let max = rules.merge_queue.max_parallel_checks as usize;
        for (base, train) in &state.trains {
            let cars = train.cars.len();
            let drafts = forge
                .open_drafts()
                .iter()
                .filter(|p| &p.base == base)
                .count();
            self.check("I4", cars <= max && drafts <= max, t, || {
                format!("{cars} cars and {drafts} drafts on {base}, max_parallel_checks {max}")
            });
        }
    }

    /// Runs once nothing is pending anymore.
    pub fn at_quiescence<G: GitStore>(
        &mut self,
        forge: &SimForge<G>,
        state: &State,
        rules: &Rules,
    ) {
        let t = forge.now();
        for (n, e) in &state.queued {
            let halted = state.trains.get(&e.base).and_then(|t| t.halted.clone());
            match halted {
                Some(reason) => self.note("I2", format!("#{n} held by a halted queue: {reason}")),
                None => self.check("I2", false, t, || {
                    format!("#{n} still queued at quiescence ({:?})", e.status)
                }),
            }
        }
        let ok = state.queued.is_empty()
            || state.queued.values().all(|e| {
                state
                    .trains
                    .get(&e.base)
                    .is_some_and(|t| t.halted.is_some())
            });
        self.check("I2", ok, t, || "queued pull requests left".into());
        self.branches(forge, state, rules, t);
        let leftover: Vec<String> = state.trains.values().flat_map(|t| t.branches()).collect();
        self.check(
            "I3",
            leftover.is_empty() || !state.queued.is_empty(),
            t,
            || format!("queue branches left after quiescence: {leftover:?}"),
        );
    }
}

/// The pull request's head is the one the car merged.
fn pr_head_in_car<G: GitStore>(
    forge: &SimForge<G>,
    car: &crate::sim::CarRecord,
    pr: &PullRequest,
) -> bool {
    if car.in_place {
        return car.head == pr.head_sha;
    }
    forge.repo.is_ancestor(&pr.head_sha, &car.head)
}

fn merge_view(pr: &PullRequest, draft: Option<&PullRequest>, head: &str) -> PullRequest {
    let mut v = pr.clone();
    if let Some(d) = draft {
        v.checks = d.checks.clone();
    }
    v.head_sha = head.to_string();
    v
}

#[cfg(test)]
mod tests {
    use super::{Checker, is_queue_branch};
    use crate::git::Tree;
    use crate::sim::SimForge;
    use schlange_config::rules::Rules;
    use schlange_core::State;

    #[test]
    fn an_untested_commit_on_main_violates_i0_and_a_stray_branch_violates_i3() {
        let rules = Rules::from_yaml("queue_rules:\n  - name: default\n").unwrap();
        let mut forge = SimForge::new("main", Tree::new(), vec![], 0);
        let state = State::default();
        let mut c = Checker::new();
        c.after_step(&forge, &state, &rules);
        assert!(!c.violated());
        let tip = forge.read_branch("main").unwrap();
        let sneaky = forge.repo.commit(
            [("x".to_string(), "untested".to_string())].into(),
            vec![tip.clone()],
            "sneaky",
        );
        forge.repo.update_ref("main", &tip, &sneaky).unwrap();
        forge
            .repo
            .create_ref("mergify/merge-queue/0123456789", &sneaky)
            .unwrap();
        c.after_step(&forge, &state, &rules);
        assert!(!c.results["I0"].violations.is_empty());
        assert!(!c.results["I3"].violations.is_empty());
    }

    #[test]
    fn strict_queue_branch_pattern() {
        let p = vec!["mergify/merge-queue/".to_string()];
        assert!(is_queue_branch("mergify/merge-queue/a76b1f3d25", &p));
        assert!(!is_queue_branch("mergify/merge-queue/a76b1f3d2", &p));
        assert!(!is_queue_branch("mergify/merge-queue/my-branch", &p));
        assert!(!is_queue_branch("mergify/merge-queue/A76B1F3D25", &p));
    }
}
