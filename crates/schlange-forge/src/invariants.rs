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
//!   - *Merge methods*: `merge` and `fast-forward` of a batch branch: each first-parent commit is
//!     the tested prefix tree (as `squash`); `rebase` and `fast-forward` in place: each pull
//!     request's last commit is the tested prefix tree, its earlier commits (`LandedCommit::
//!     extra`) form one chain on the expected parent; `merge-batch`: one merge commit for the
//!     batch whose tree is the batch's tested tree.
//!   - *Modes `parallel` and `isolated`*: a car that does not depend on one that landed lands
//!     as its tested change on the actual parent, with only other lanes' landings in between
//!     (isolated: any; parallel: those in scopes the car does not touch).
//!   - *`skip_intermediate_results`*: the batch semantics apply also for batch size 1.
//! - **I1** No pull request lands whose queue or merge conditions were false at landing time
//!   (on the forge's truth), whose head is not the head recorded when its car was built (the
//!   tested head), or that already landed (nothing lands twice, also across crashes); nor while
//!   the queue is paused, a freeze holds it or a merge protection fails (the operator controls
//!   the simulated forge delivered before the landing); required checks of the base count.
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

use schlange_config::rules::MergeMethod;
use schlange_core::engine::LandedCommit;

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
    /// The pull request each queue landing commit belongs to.
    landed_by: BTreeMap<Sha, u64>,
    landed_prs: BTreeMap<u64, u64>,
    /// Bases where a landing's real tree differed from the tree the queue branch had when it
    /// was landed: a halt there is the tree check doing its job (MQ-02).
    tree_mismatch: BTreeSet<String>,
    noted_modes: bool,
    /// Queue entries as last observed (queue, since, seq, base, by step): the queue attributes
    /// (`queue-name`, `queue-position`, `queued-at`) are facts about the queue, not the forge,
    /// so the checker takes them from what it saw before the landing step.
    queue_seen: BTreeMap<u64, (String, u64, u64, String)>,
    epoch: u64,
    /// Scopes (and barrier flag) of every landed pull request, from its files right before it
    /// landed (parallel mode: what may land between a car's base and its landing).
    landed_scopes: BTreeMap<u64, (Vec<String>, bool)>,
}

pub const IDS: [&str; 6] = ["I0", "I1", "I2", "I3", "I4", "I5"];

fn prefixes(rules: &Rules) -> Vec<String> {
    rules
        .queue_rules
        .iter()
        .map(|q| q.queue_branch_prefix.clone())
        .collect()
}

/// Strict per-commit I0 applies when no queue batches. With `skip_intermediate_results` a
/// green car lands the cars ahead of it with it (Mergify's semantics: the passing car stands in
/// for their checks), so the batch semantics apply instead.
fn strict_i0(rules: &Rules) -> bool {
    rules.queue_rules.iter().all(|q| q.batch_size.max() == 1)
        && !rules.merge_queue.skip_intermediate_results
}

/// The first commit a landed commit put on the base (its replayed predecessors come first).
fn entry(c: &LandedCommit) -> &str {
    c.extra.first().unwrap_or(&c.sha)
}

/// Whether the configuration lets a landing sit on a base that moved since its car was built
/// (the tested change applied to the actual parent): `reset_on_external_merge: never`, and
/// the `parallel` and `isolated` modes, whose cars do not wait for one another.
fn drift_allowed(rules: &Rules) -> bool {
    rules.merge_queue.reset_on_external_merge == "never" || rules.merge_queue.mode != "serial"
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

    /// The evaluation context at a landing: the queue attributes as observed before the landing
    /// step (a pull request queued and landed within one step counts as queued at `t`, first in
    /// its queue). The repository and clock are the simulation's.
    fn queue_context(&self, number: u64, queue: &str, t: u64) -> Context {
        let epoch = self.epoch as i64;
        let (name, since, position) = match self.queue_seen.get(&number) {
            Some((q, since, seq, base)) => {
                let pos = self
                    .queue_seen
                    .values()
                    .filter(|(oq, _, os, ob)| oq == q && ob == base && os < seq)
                    .count() as i64;
                (q.clone(), *since, pos)
            }
            None => (queue.to_string(), t, 0),
        };
        Context {
            now: epoch + t as i64,
            repository: crate::runner::SIM_REPOSITORY.into(),
            queue: Some(eval::QueueAttrs {
                name,
                position,
                queued_at: epoch + since as i64,
                merge_started_at: None,
            }),
            ..Context::default()
        }
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
        self.epoch = state.host.epoch;
        self.queue_seen = state
            .queued
            .iter()
            .map(|(n, e)| (*n, (e.queue.clone(), e.since, e.seq, e.base.clone())))
            .collect();
        self.base_history(forge, t);
        self.branches(forge, state, rules, t, false);
        self.parallel(forge, state, rules, t);
    }

    fn landings<G: GitStore>(&mut self, forge: &SimForge<G>, rules: &Rules, t: u64) {
        let new: Vec<_> = forge.landings[self.landings_seen..].to_vec();
        self.landings_seen = forge.landings.len();
        let never = drift_allowed(rules);
        let isolated = rules.merge_queue.mode == "isolated";
        let parallel = rules.merge_queue.mode == "parallel";
        let ext_ok = rules.merge_queue.reset_on_external_merge == "never";
        let start = self.landings_seen - new.len();
        for (k, l) in new.into_iter().enumerate() {
            let idx = start + k;
            if l.commits.is_empty() {
                continue;
            }
            for p in &l.before {
                let sc = rules
                    .scopes
                    .as_ref()
                    .map(|s| s.of(&p.files))
                    .unwrap_or_default();
                self.landed_scopes.insert(p.number, sc);
            }
            // I1: nothing lands while the queue is paused or a freeze holds the pull request,
            // and every merge protection whose `if` holds is satisfied (on the forge's truth).
            let (paused, freezes) = forge.controls_before(idx);
            self.check("I1", paused.is_none(), t, || {
                format!("car {} landed while the merge queue was paused", l.car)
            });
            for p in &l.before {
                let ctx = self.queue_context(p.number, &l.queue, t);
                for f in &freezes {
                    let held = f.holds(p, &ctx, l.at);
                    self.check("I1", !held, t, || {
                        format!("#{} landed while freeze `{}` held it", p.number, f.reason)
                    });
                }
                for m in &rules.merge_protections {
                    let applies = eval::eval_list(&m.when, p, &ctx, &mut vec![]).0;
                    let ok =
                        !applies || eval::eval_list(&m.success_conditions, p, &ctx, &mut vec![]).0;
                    self.check("I1", ok, t, || {
                        format!(
                            "#{} landed while merge protection `{}` failed",
                            p.number, m.name
                        )
                    });
                }
            }
            for c in &l.commits {
                self.landed_commits.insert(c.sha.clone());
                self.landed_commits.extend(c.extra.iter().cloned());
                self.landed_by.insert(c.sha.clone(), c.number);
                for x in &c.extra {
                    self.landed_by.insert(x.clone(), c.number);
                }
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
            let required = forge
                .required_checks
                .get(&l.base_branch)
                .cloned()
                .unwrap_or_default();
            let merge_conds = rule.map(|r| {
                let mut v = if r.merge_conditions.is_empty() {
                    r.queue_conditions.clone()
                } else {
                    r.merge_conditions.clone()
                };
                if r.injection_mode != schlange_config::rules::InjectionMode::None {
                    v.extend(required_conditions(&required));
                }
                v
            });
            let queue_conds = rule.map(|r| {
                let mut v = r.queue_conditions.clone();
                if r.injection_mode == schlange_config::rules::InjectionMode::Queue {
                    v.extend(required_conditions(&required));
                }
                v
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
                let ctx = self.queue_context(c.number, &l.queue, t);
                let qc = queue_conds
                    .clone()
                    .unwrap_or_else(|| rule.queue_conditions.clone());
                let (q_ok, _) = eval::eval_list(&qc, before, &ctx, &mut vec![]);
                self.check("I1", q_ok, t, || {
                    format!("#{} landed while its queue_conditions were false", c.number)
                });
                let view = merge_view(before, l.draft_before.as_ref(), &car.head);
                let (m_ok, _) = eval::eval_list(conds, &view, &ctx, &mut vec![]);
                self.check("I1", m_ok, t, || {
                    format!("#{} landed while its merge conditions were false", c.number)
                });
                green &= m_ok;
                // Green is not vacuous: the conditions were evaluated on the car's draft at
                // exactly the car's head (or in place on the pull request at that head). With
                // named checks this ties "green" to runs on the tested commit; without them it
                // is the only evidence the queue branch was checked at all.
                if eval::referenced_checks(conds).is_empty() {
                    self.note(
                        "I0",
                        "merge conditions name no check: green means the conditions held on \
                         the car's draft (or in place) right before landing"
                            .into(),
                    );
                }
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
                        "#{} landed although the draft's head was not the car's tested head {} \
                         (or the conditions were not evaluated on a queue branch)",
                        c.number,
                        &car.head[..car.head.len().min(7)]
                    )
                });
            }
            // I0: every landed commit is the tested prefix, on the assumed base.
            let first = l.commits[0].number;
            let Some(idx0) = car.prefix.iter().position(|(n, ..)| *n == first) else {
                self.check("I0", false, t, || format!("#{first} is not in its car"));
                continue;
            };
            let parent = forge.repo.parents(entry(&l.commits[0])).first().cloned();
            let expected_parent = if idx0 == 0 {
                car.base_sha.clone()
            } else {
                car.prefix[idx0 - 1].1.clone()
            };
            let expected_parent_tree = forge.repo.tree_id_of(&expected_parent);
            let parent_ok = parent
                .as_ref()
                .is_some_and(|p| forge.repo.tree_id_of(p) == expected_parent_tree);
            // The tree check of the engine (MQ-02): a landing whose real tree is not the tree
            // the queue branch had when it was landed justifies a halt.
            let tested_tip = if car.in_place {
                l.before.first().map(|p| p.head_sha.clone())
            } else {
                l.draft_before.as_ref().map(|d| d.head_sha.clone())
            };
            if let (Some(tip), Some(last)) = (tested_tip, l.commits.last())
                && forge.repo.tree_id_of(&tip) != forge.repo.tree_id_of(&last.sha)
            {
                self.tree_mismatch.insert(l.base_branch.clone());
            }
            if !parent_ok && never && parent.is_some() {
                // The base moved under the car: only commits made outside the queue (or the
                // landings of the pull requests ahead in this car) may lie between the base it
                // was tested on and the actual parent; the user opted out of retesting after
                // external merges, nothing else.
                let ahead: Vec<u64> = car.prefix[..idx0].iter().map(|(n, ..)| *n).collect();
                // Scopes of this landing's pull requests (parallel mode).
                let mine: Vec<(Vec<String>, bool)> = l
                    .commits
                    .iter()
                    .filter_map(|c| self.landed_scopes.get(&c.number).cloned())
                    .collect();
                let disjoint = |n: u64| {
                    let Some((sc, barrier)) = self.landed_scopes.get(&n) else {
                        return false;
                    };
                    !barrier
                        && mine
                            .iter()
                            .all(|(m, b)| !b && !m.iter().any(|x| sc.contains(x)))
                };
                let mut reached = false;
                let mut foreign = Vec::new();
                for sha in forge
                    .repo
                    .first_parent_history(parent.as_deref().unwrap_or_default())
                {
                    if sha == car.base_sha {
                        reached = true;
                        break;
                    }
                    let external = ext_ok && forge.external_commits.contains(&sha);
                    let by = self.landed_by.get(&sha).copied();
                    let ahead_landing = by.is_some_and(|n| ahead.contains(&n));
                    // Isolated mode: any other queue landing; parallel: landings in scopes
                    // this car does not touch.
                    let other_lane = by.is_some_and(|n| isolated || (parallel && disjoint(n)));
                    if !external && !ahead_landing && !other_lane {
                        foreign.push(sha[..7].to_string());
                    }
                }
                self.check("I0", reached && foreign.is_empty(), t, || {
                    format!(
                        "drift: #{first} of car {} landed on a parent that is not its tested \
                         base plus commits it may skip (external with \
                         reset_on_external_merge: never, other lanes in parallel or isolated \
                         mode) (base reached: {reached}; other commits: {foreign:?})",
                        car.id
                    )
                });
                // `reset_on_external_merge: never`: the tested change on the actual parent.
                self.note(
                    "I0",
                    "drift (reset_on_external_merge: never, parallel or isolated mode): a \
                     landing on a base that moved since its car was built is checked as the \
                     tested change applied to the actual parent"
                        .into(),
                );
                let mut prev_tested = expected_parent;
                let batch_merge = l.method == MergeMethod::MergeBatch;
                for (k, c) in l.commits.iter().enumerate() {
                    if batch_merge && k + 1 < l.commits.len() {
                        continue; // one merge commit for the whole batch: checked once
                    }
                    let k = if batch_merge { l.commits.len() - 1 } else { k };
                    let Some((n, tested, _)) = car.prefix.get(idx0 + k).cloned() else {
                        self.check("I0", false, t, || {
                            format!("#{} is not in its car", c.number)
                        });
                        break;
                    };
                    let actual_parent = forge.repo.parents(entry(c)).first().cloned();
                    // The repository's own three-way merge (line level on real git).
                    let expected = actual_parent
                        .and_then(|p| forge.repo.merged_tree_id(&prev_tested, &p, &tested).ok());
                    let ok = n == c.number && expected == Some(forge.repo.tree_id_of(&c.sha));
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
            if l.method == MergeMethod::MergeBatch {
                // One merge commit of the batch branch: the batch's tested tip tree, parent the
                // expected base; every pull request of the car names that commit.
                self.note(
                    "I0",
                    "merge-batch: one merge commit per batch whose tree is the tested batch tree"
                        .into(),
                );
                let sha = &l.commits[0].sha;
                let real_tree = forge.repo.tree_id_of(sha);
                let last_tree = car
                    .prefix
                    .get(idx0 + l.commits.len() - 1)
                    .map(|(_, _, tr)| tr.clone());
                let ok = l.commits.iter().all(|c| c.sha == *sha)
                    && Some(real_tree) == last_tree
                    && l.commits.iter().enumerate().all(|(k, c)| {
                        car.prefix
                            .get(idx0 + k)
                            .is_some_and(|(n, ..)| *n == c.number)
                    });
                self.check("I0", ok, t, || {
                    format!(
                        "merge-batch commit {} is not car {}'s tested tree",
                        &sha[..7],
                        car.id
                    )
                });
                self.check("I0", green, t, || {
                    format!("car {} landed without green merge checks", car.id)
                });
                continue;
            }
            for (k, c) in l.commits.iter().enumerate() {
                if k > 0 {
                    // Each pull request's commits sit right on the one before it.
                    let p = forge.repo.parents(entry(c)).first().cloned();
                    self.check(
                        "I0",
                        p.as_deref() == Some(l.commits[k - 1].sha.as_str()),
                        t,
                        || format!("#{} does not sit on the commit before it", c.number),
                    );
                }
                for (x, w) in c.extra.iter().zip(c.extra.iter().skip(1).chain([&c.sha])) {
                    let p = forge.repo.parents(w).first().cloned();
                    self.check("I0", p.as_deref() == Some(x.as_str()), t, || {
                        format!("the replayed commits of #{} are not one chain", c.number)
                    });
                }
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
        // A halt holds pull requests on purpose only when a landing really did not match the
        // tested tree; any other halt with pull requests left is a wedge (I2).
        let mismatch = self.tree_mismatch.clone();
        let justified = |base: &str| mismatch.contains(base);
        let mut wedged = Vec::new();
        for (n, e) in &state.queued {
            let halted = state.trains.get(&e.base).and_then(|t| t.halted.clone());
            match halted {
                Some(reason) if justified(&e.base) => {
                    self.note("I2", format!("#{n} held by a halted queue: {reason}"))
                }
                Some(reason) => wedged.push(format!(
                    "#{n} held by a halt the forge's landings do not justify: {reason}"
                )),
                None => wedged.push(format!("#{n} still queued at quiescence ({:?})", e.status)),
            }
        }
        for w in &wedged {
            self.check("I2", false, t, || w.clone());
        }
        self.check("I2", wedged.is_empty(), t, || {
            "queued pull requests left".into()
        });
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

fn required_conditions(checks: &[String]) -> Vec<schlange_config::conditions::Condition> {
    let v = serde_json::Value::Array(
        checks
            .iter()
            .map(|c| serde_json::Value::String(format!("check-success = {c}")))
            .collect(),
    );
    schlange_config::conditions::parse_list(&v, "", &mut Vec::new())
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

    /// Critic 1abc: a halt must not hide a wedge. A halted queue with pull requests left and no
    /// landing whose tree differed from the tested tree violates I2.
    #[test]
    fn an_unjustified_halt_is_a_wedge() {
        use schlange_core::engine::{EntryStatus, QueueEntry};
        let rules = Rules::from_yaml("queue_rules:\n  - name: default\n").unwrap();
        let forge = SimForge::new("main", Tree::new(), vec![], 0);
        let mut state = State::default();
        let mut train = schlange_core::queue::Train::new("main");
        train.halted = Some("no reason".into());
        state.trains.insert("main".into(), train);
        state.queued.insert(
            1,
            QueueEntry {
                queue: "default".into(),
                rule: "r".into(),
                since: 0,
                head_sha: "h".into(),
                base: "main".into(),
                seq: 1,
                waiting_since: 0,
                status: EntryStatus::Waiting,
                by: None,
                priority: 2000,
                priority_rule: None,
                interrupt: false,
            },
        );
        let mut c = Checker::new();
        c.at_quiescence(&forge, &state, &rules);
        assert!(
            !c.results["I2"].violations.is_empty(),
            "{:?}",
            c.results["I2"]
        );
    }

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
