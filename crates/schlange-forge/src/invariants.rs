//! Invariants I0–I5 (KONZEPT.md, section 6), checked by the scenario runner after every step
//! with the full knowledge of the simulated forge. The checker never asks the engine whether it
//! did the right thing: it judges from the forge's records (cars built, landings, commits).
//!
//! - **I0** Every commit on a base branch is the initial commit, a commit made outside the
//!   queue, or a queue landing whose tree equals the tree the queue built and tested for exactly
//!   that prefix, on top of exactly the base it assumed, and the car was green.
//!   - *Batches* (Mergify's semantics): with `squash`, the commits of one batch are prefixes of
//!     the tested batch branch, and checks ran only on its tip. Each commit is the tested
//!     prefix tree; only the tip tree had checks of its own.
//!   - *Strict per commit* (every queue rule has batch size 1): every landed commit's tree is a
//!     tree on which the checks ran (the car's tip), one commit per landing.
//!   - *Green is never vacuous*: when the merge conditions name no check, the tree must still
//!     have been a car whose draft existed on the queue branch (or the pull request itself in
//!     place) and the merge conditions held on that view right before landing.
//!   - *`reset_on_external_merge: never`*: the user opts out of retesting after an external
//!     commit. A landing on a base that moved outside the queue must then equal the tested
//!     change applied to the actual parent (three-way merge of tested base, actual parent and
//!     tested prefix), not the tested tree itself.
//! - **I1** No pull request lands whose queue or merge conditions were false at landing time
//!   (on the forge's truth), whose head is not the head recorded when its car was built (the
//!   tested head), or that already landed (nothing lands twice, also across crashes).
//! - **I2** At quiescence every queued pull request was merged or dequeued with a reason; a
//!   queue that halted on purpose (tree mismatch) is reported with its reason.
//! - **I3** The queue branches on the forge (prefix plus ten hex characters) and the open queue
//!   drafts are exactly those of the engine's cars: after every step, and at quiescence. With
//!   forge faults or crashes a side effect can be in flight or in doubt, so after every step
//!   only "everything on the forge is known to the engine (a car or a close in progress)" is
//!   checked, and the exact equality at quiescence (KONZEPT's definition).
//! - **I4** At no step more than `max_parallel_checks` cars per base in the engine (cars plus
//!   cars being closed) and on the forge (queue drafts that are open or whose CI still runs
//!   after closing).
//! - **I5** Same end state (merged pull requests, outcomes with dequeue reasons, base trees,
//!   pull requests left queued) with perfect and with disturbed delivery; compared by the
//!   runner and the property tests.
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
use schlange_core::queue::{closing_count, is_queue_branch};

use crate::git::{GitStore, Repo, Sha};
use crate::sim::{BOT, SimForge};

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
    /// Forge faults or crashes: I3 per step checks only that the engine knows everything on
    /// the forge; the exact equality is checked at quiescence.
    pub relaxed_steps: bool,
    landings_seen: usize,
    accounted: BTreeSet<Sha>,
    landed_commits: BTreeSet<Sha>,
    landed_prs: BTreeMap<u64, u64>,
    noted_modes: bool,
}

pub const IDS: [&str; 6] = ["I0", "I1", "I2", "I3", "I4", "I5"];

fn prefixes(rules: &Rules) -> Vec<String> {
    rules
        .queue_rules
        .iter()
        .map(|q| q.queue_branch_prefix.clone())
        .collect()
}

/// Strict per-commit I0 applies when no queue batches.
fn strict_i0(rules: &Rules) -> bool {
    rules.queue_rules.iter().all(|q| q.batch_size.max() == 1)
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
        let notes = &mut self.results.get_mut(id).expect("known id").notes;
        if !notes.contains(&msg) {
            notes.push(msg);
        }
    }

    pub fn violated(&self) -> bool {
        self.results.values().any(|r| !r.violations.is_empty())
    }

    /// Runs after every simulated step, once the engine's decisions were carried out.
    pub fn after_step<G: GitStore>(&mut self, forge: &SimForge<G>, state: &State, rules: &Rules) {
        let t = forge.now();
        if !self.noted_modes {
            self.noted_modes = true;
            if strict_i0(rules) {
                self.note("I0", "strict per-commit mode (batch size 1)".into());
            } else {
                self.note(
                    "I0",
                    "batch mode: commits inside a batch are tested prefixes, checks ran on the tip"
                        .into(),
                );
            }
            if self.relaxed_steps {
                self.note(
                    "I3",
                    "forge faults or crashes: per step only \"the engine knows every queue branch \
                     and draft\"; exact equality at quiescence"
                        .into(),
                );
            }
        }
        self.landings(forge, rules, t);
        self.base_history(forge, t);
        self.branches(forge, state, rules, t, false);
        self.parallel(forge, state, rules, t);
    }

    fn landings<G: GitStore>(&mut self, forge: &SimForge<G>, rules: &Rules, t: u64) {
        let new: Vec<_> = forge.landings[self.landings_seen..].to_vec();
        self.landings_seen = forge.landings.len();
        let never = rules.merge_queue.reset_on_external_merge == "never";
        for l in new {
            if l.commits.is_empty() {
                continue;
            }
            for c in &l.commits {
                self.landed_commits.insert(c.sha.clone());
                let first = self.landed_prs.insert(c.number, l.car);
                self.check("I1", first.is_none(), t, || {
                    format!(
                        "#{} landed twice (cars {} and {})",
                        c.number,
                        first.unwrap_or(0),
                        l.car
                    )
                });
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
                let tested = car.heads.iter().find(|(n, _)| *n == c.number);
                let heads_ok = match (before, requested, tested) {
                    (Some(b), Some(r), Some((_, h))) => {
                        b.head_sha == r.head_sha
                            && r.head_sha == *h
                            && pr_head_in_car(forge, car, b)
                    }
                    _ => false,
                };
                self.check("I1", heads_ok, t, || {
                    format!(
                        "#{} landed with a head that was not the tested head",
                        c.number
                    )
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
                // Green is not vacuous: without a named check, the conditions must at least
                // have been evaluated on the queue branch's draft (or in place).
                if eval::referenced_checks(conds).is_empty() {
                    self.note(
                        "I0",
                        "merge conditions name no check: green means the conditions held on \
                         the car's draft (or in place) right before landing"
                            .into(),
                    );
                    let evaluated = if car.in_place {
                        car.head == before.head_sha
                    } else {
                        l.draft_before.as_ref().is_some_and(|d| {
                            d.head_sha == car.head
                                && d.head_ref == car.branch.clone().unwrap_or_default()
                        })
                    };
                    self.check("I0", evaluated, t, || {
                        format!(
                            "#{} landed without its conditions evaluated on a queue branch",
                            c.number
                        )
                    });
                }
            }
            // I0: every landed commit is the tested prefix, on the assumed base.
            let first = l.commits[0].number;
            let Some(idx0) = car.prefix.iter().position(|(n, ..)| *n == first) else {
                self.check("I0", false, t, || format!("#{first} is not in its car"));
                continue;
            };
            let parent = forge.repo.parents(&l.commits[0].sha).first().cloned();
            let expected_parent = if idx0 == 0 {
                car.base_sha.clone()
            } else {
                car.prefix[idx0 - 1].1.clone()
            };
            let expected_parent_tree = forge.repo.tree_id_of(&expected_parent);
            let parent_ok = parent
                .as_ref()
                .is_some_and(|p| forge.repo.tree_id_of(p) == expected_parent_tree);
            if !parent_ok && never && parent.is_some() {
                // `reset_on_external_merge: never`: the tested change on the actual parent.
                self.note(
                    "I0",
                    "reset_on_external_merge: never: a landing after an external commit is \
                     checked as the tested change applied to the actual parent"
                        .into(),
                );
                let mut prev_tested = expected_parent;
                for (k, c) in l.commits.iter().enumerate() {
                    let Some((n, tested, _)) = car.prefix.get(idx0 + k).cloned() else {
                        self.check("I0", false, t, || {
                            format!("#{} is not in its car", c.number)
                        });
                        break;
                    };
                    let actual_parent = forge.repo.parents(&c.sha).first().cloned();
                    let expected = actual_parent.and_then(|p| {
                        Repo::merge_trees(
                            &forge.repo.tree(&prev_tested),
                            &forge.repo.tree(&p),
                            &forge.repo.tree(&tested),
                        )
                        .ok()
                    });
                    let ok = n == c.number && expected == Some(forge.repo.tree(&c.sha));
                    self.check("I0", ok, t, || {
                        format!(
                            "commit {} of #{} is not the tested change on its parent",
                            &c.sha[..7],
                            c.number
                        )
                    });
                    prev_tested = tested;
                }
                self.check("I0", green, t, || {
                    format!("car {} landed without green merge checks", car.id)
                });
                continue;
            }
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
            if strict_i0(rules) {
                let tip = l
                    .commits
                    .last()
                    .map(|c| forge.repo.tree_id_of(&c.sha))
                    .unwrap_or_default();
                let ok = l.commits.len() == 1 && tip == car.tree;
                self.check("I0", ok, t, || {
                    format!(
                        "strict mode: car {} landed {} commits; the landed tree is not the tree \
                         its checks ran on",
                        car.id,
                        l.commits.len()
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

    fn branches<G: GitStore>(
        &mut self,
        forge: &SimForge<G>,
        state: &State,
        rules: &Rules,
        t: u64,
        quiescent: bool,
    ) {
        let prefixes = prefixes(rules);
        let on_forge: BTreeSet<String> = forge
            .repo
            .ref_names()
            .into_iter()
            .filter(|r| is_queue_branch(r, &prefixes))
            .collect();
        let in_use: BTreeSet<String> = state.trains.values().flat_map(|t| t.branches()).collect();
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
        if self.relaxed_steps && !quiescent {
            let mut known = in_use.clone();
            known.extend(state.closing.values().filter_map(|c| c.branch.clone()));
            let unknown: Vec<&String> = on_forge.difference(&known).collect();
            self.check("I3", unknown.is_empty(), t, || {
                format!("queue branches on the forge the engine does not know: {unknown:?}")
            });
            let stray: Vec<u64> = forge
                .open_drafts()
                .iter()
                .filter(|p| {
                    !known.contains(&p.head_ref)
                        && !car_drafts.contains(&p.number)
                        && !state.closing.values().any(|c| c.draft == Some(p.number))
                })
                .map(|p| p.number)
                .collect();
            self.check("I3", stray.is_empty(), t, || {
                format!("open queue drafts the engine does not know: {stray:?}")
            });
            return;
        }
        self.check("I3", on_forge == in_use, t, || {
            format!("queue branches on the forge {on_forge:?} ≠ branches in use {in_use:?}")
        });
        self.check("I3", drafts == car_drafts, t, || {
            format!("open draft pull requests {drafts:?} ≠ drafts of cars {car_drafts:?}")
        });
    }

    fn parallel<G: GitStore>(&mut self, forge: &SimForge<G>, state: &State, rules: &Rules, t: u64) {
        let max = rules.merge_queue.max_parallel_checks as usize;
        let mut bases: BTreeSet<String> = state.trains.keys().cloned().collect();
        bases.extend(forge.base_branches.iter().cloned());
        for base in bases {
            let cars = state.trains.get(&base).map_or(0, |t| t.cars.len());
            let closing = closing_count(state, &base);
            let drafts = forge.drafts_with_ci(&base);
            self.check("I4", cars + closing <= max && drafts <= max, t, || {
                format!(
                    "{cars} cars + {closing} closing in the engine, {drafts} queue drafts open or \
                     running CI on {base}; max_parallel_checks {max}"
                )
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
        self.branches(forge, state, rules, t, true);
        let leftover: Vec<String> = state.trains.values().flat_map(|t| t.branches()).collect();
        self.check(
            "I3",
            leftover.is_empty() || !state.queued.is_empty(),
            t,
            || format!("queue branches left after quiescence: {leftover:?}"),
        );
        self.check("I3", state.closing.is_empty(), t, || {
            format!("closes never confirmed: {:?}", state.closing.keys())
        });
        let running: Vec<u64> = forge
            .base_branches
            .iter()
            .flat_map(|b| {
                forge
                    .open_drafts()
                    .into_iter()
                    .filter(move |p| &p.base == b && p.author == BOT)
                    .map(|p| p.number)
                    .collect::<Vec<_>>()
            })
            .collect();
        self.check(
            "I4",
            running.is_empty() || !state.queued.is_empty(),
            t,
            || format!("queue drafts still open at quiescence: {running:?}"),
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
    use super::Checker;
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
        // Relaxed (forge faults): a branch the engine does not know is still a violation.
        let mut r = Checker::new();
        r.relaxed_steps = true;
        r.after_step(&forge, &state, &rules);
        assert!(!r.results["I3"].violations.is_empty());
    }
}
