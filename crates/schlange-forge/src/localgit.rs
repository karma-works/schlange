//! The `localgit` forge: the simulated forge's logic on real git repositories on disk, in the
//! spirit of Prow's `localgit` and the "git" rung of uber/submitqueue's provider ladder
//! (fake → git → github).
//!
//! Layout of one instance (a fresh temporary directory, removed on drop):
//!
//! - `remote.git`: a bare repository, the "remote" the forge serves. Base branches, pull
//!   request heads (`pr-<n>`) and queue branches are `refs/heads/*` in it.
//! - `work/`: the working area the adapter controls. Every check run gets a fresh
//!   `git worktree add --detach` checkout of the tested commit here, runs `sh -c <command>`
//!   in it and is removed afterwards.
//!
//! What is real: commits, trees and blobs are git objects (`git hash-object`, `write-tree`,
//! `commit-tree`); merges and squashes are git's three-way merge (`git merge-tree
//! --write-tree`, the `ort` strategy, line-level with git's conflict detection); every ref
//! write is a compare-and-swap with `git update-ref <ref> <new> <old>` (creation expects the
//! all-zero id: the ref must not exist); nothing is ever forced. All reads (refs, history,
//! ancestry, merge bases, changed paths, trees) go to the repository. Pull request metadata
//! (number, author, title, labels, draft, check runs), the clock and webhook delivery are the
//! adapter's own, as with any fake forge: see [`crate::sim`].
//!
//! git runs with an empty configuration (`GIT_CONFIG_GLOBAL=/dev/null`,
//! `GIT_CONFIG_NOSYSTEM=1`) and fixed author, committer and dates, so commit ids are
//! deterministic and a developer's settings (signing, hooks) do not leak in.
//!
//! The release binary does not need this module: it sits behind the cargo feature `localgit`
//! (default for this crate, off for `schlange` unless its own `localgit` feature is on). The
//! container image ships no git.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use crate::ci::{TempDir, run_shell};
use crate::git::{Conflict, GitStore, RefError, Sha, Tree};
use crate::runner::{RunReport, Scenario, run_with};
use crate::scenario::ScenarioError;

const ZERO: &str = "0000000000000000000000000000000000000000";
const DATE: &str = "1700000000 +0000";

#[derive(Debug, thiserror::Error)]
pub enum LocalGitError {
    #[error("git is not usable: {0}")]
    NoGit(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Scenario(#[from] ScenarioError),
}

#[derive(Default)]
struct Cache {
    /// Commit → (tree id, parents). Commits are immutable.
    commits: HashMap<Sha, (String, Vec<Sha>)>,
    trees: HashMap<String, Tree>,
    ancestor: HashMap<(Sha, Sha), bool>,
    merge_base: HashMap<(Sha, Sha), Option<Sha>>,
    changed: HashMap<(Sha, Sha), Vec<String>>,
    history: HashMap<Sha, Vec<Sha>>,
}

/// Real git repositories on disk behind the [`GitStore`] interface.
pub struct LocalGit {
    root: TempDir,
    remote: PathBuf,
    work: PathBuf,
    blobs: HashMap<String, String>,
    cache: RefCell<Cache>,
    checkouts: Cell<u64>,
    /// git processes started.
    pub git_calls: Cell<u64>,
}

impl std::fmt::Debug for LocalGit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalGit")
            .field("remote", &self.remote)
            .finish()
    }
}

fn git_command() -> Command {
    let mut c = Command::new("git");
    for var in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
    ] {
        c.env_remove(var);
    }
    c.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .env("GIT_AUTHOR_NAME", "Schlange")
        .env("GIT_AUTHOR_EMAIL", "schlange@localhost")
        .env("GIT_AUTHOR_DATE", DATE)
        .env("GIT_COMMITTER_NAME", "Schlange")
        .env("GIT_COMMITTER_EMAIL", "schlange@localhost")
        .env("GIT_COMMITTER_DATE", DATE);
    c
}

/// Whether a usable git (with `merge-tree --write-tree`, git ≥ 2.38) is on the path.
pub fn available() -> Result<(), LocalGitError> {
    let out = git_command()
        .arg("--version")
        .output()
        .map_err(|e| LocalGitError::NoGit(e.to_string()))?;
    let v = String::from_utf8_lossy(&out.stdout);
    let nums: Vec<u32> = v
        .split_whitespace()
        .nth(2)
        .unwrap_or("")
        .split('.')
        .filter_map(|p| p.parse().ok())
        .collect();
    match nums.as_slice() {
        [major, minor, ..] if (*major, *minor) >= (2, 38) => Ok(()),
        _ => Err(LocalGitError::NoGit(format!(
            "need git ≥ 2.38 for `merge-tree --write-tree`, found `{}`",
            v.trim()
        ))),
    }
}

impl LocalGit {
    /// A fresh bare repository and working area in a new temporary directory.
    pub fn new() -> Result<Self, LocalGitError> {
        available()?;
        let root = TempDir::new("schlange-localgit")?;
        let remote = root.path().join("remote.git");
        let work = root.path().join("work");
        std::fs::create_dir_all(&work)?;
        let out = git_command()
            .args(["init", "--bare", "-q"])
            .arg(&remote)
            .output()?;
        if !out.status.success() {
            return Err(LocalGitError::NoGit(
                String::from_utf8_lossy(&out.stderr).into_owned(),
            ));
        }
        let g = LocalGit {
            root,
            remote,
            work,
            blobs: HashMap::new(),
            cache: RefCell::new(Cache::default()),
            checkouts: Cell::new(0),
            git_calls: Cell::new(0),
        };
        g.ok(&["config", "gc.auto", "0"]);
        g.ok(&["config", "core.autocrlf", "false"]);
        Ok(g)
    }

    /// The bare repository.
    pub fn remote_path(&self) -> &Path {
        &self.remote
    }

    pub fn root_path(&self) -> &Path {
        self.root.path()
    }

    fn cmd(&self) -> Command {
        self.git_calls.set(self.git_calls.get() + 1);
        let mut c = git_command();
        c.arg("--git-dir").arg(&self.remote);
        c
    }

    /// Runs git with `args` (and `stdin`), returns the whole output.
    pub fn raw(&self, args: &[&str], stdin: Option<&[u8]>, env: &[(&str, &str)]) -> Output {
        let mut c = self.cmd();
        c.args(args);
        for (k, v) in env {
            c.env(k, v);
        }
        c.stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        let mut child = c
            .spawn()
            .unwrap_or_else(|e| panic!("localgit: cannot start git {args:?}: {e}"));
        if let Some(input) = stdin {
            let mut pipe = child.stdin.take().expect("stdin");
            pipe.write_all(input)
                .unwrap_or_else(|e| panic!("localgit: git {args:?} stdin: {e}"));
        }
        child
            .wait_with_output()
            .unwrap_or_else(|e| panic!("localgit: git {args:?}: {e}"))
    }

    /// Runs git and returns trimmed stdout; a failure is a broken environment and panics.
    fn ok(&self, args: &[&str]) -> String {
        self.ok_with(args, None, &[])
    }

    fn ok_with(&self, args: &[&str], stdin: Option<&[u8]>, env: &[(&str, &str)]) -> String {
        let out = self.raw(args, stdin, env);
        if !out.status.success() {
            panic!(
                "localgit: git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        String::from_utf8_lossy(&out.stdout).trim_end().to_string()
    }

    fn full_ref(name: &str) -> String {
        format!("refs/heads/{name}")
    }

    fn blob(&mut self, content: &str) -> String {
        if let Some(oid) = self.blobs.get(content) {
            return oid.clone();
        }
        let oid = self.ok_with(
            &["hash-object", "-w", "--stdin"],
            Some(content.as_bytes()),
            &[],
        );
        self.blobs.insert(content.to_string(), oid.clone());
        oid
    }

    fn write_tree(&mut self, tree: &Tree) -> String {
        let mut info = String::new();
        for (path, content) in tree {
            let oid = self.blob(content);
            info.push_str(&format!("100644 blob {oid}\t{path}\n"));
        }
        let index = self.root.path().join("index.tmp");
        let _ = std::fs::remove_file(&index);
        let index_s = index.to_string_lossy().into_owned();
        let env = [("GIT_INDEX_FILE", index_s.as_str())];
        self.ok_with(
            &["update-index", "--add", "--index-info"],
            Some(info.as_bytes()),
            &env,
        );
        let id = self.ok_with(&["write-tree"], None, &env);
        let _ = std::fs::remove_file(&index);
        id
    }

    fn commit_tree(&mut self, tree_id: &str, parents: &[Sha], message: &str) -> Sha {
        let mut args: Vec<&str> = vec!["commit-tree", tree_id];
        for p in parents {
            args.push("-p");
            args.push(p);
        }
        args.push("-m");
        args.push(message);
        let sha = self.ok(&args);
        self.cache
            .borrow_mut()
            .commits
            .insert(sha.clone(), (tree_id.to_string(), parents.to_vec()));
        sha
    }

    fn commit_info(&self, sha: &str) -> (String, Vec<Sha>) {
        if let Some(i) = self.cache.borrow().commits.get(sha) {
            return i.clone();
        }
        let text = self.ok(&["cat-file", "commit", sha]);
        let mut tree = String::new();
        let mut parents = Vec::new();
        for line in text.lines() {
            if line.is_empty() {
                break;
            }
            if let Some(t) = line.strip_prefix("tree ") {
                tree = t.to_string();
            } else if let Some(p) = line.strip_prefix("parent ") {
                parents.push(p.to_string());
            }
        }
        self.cache
            .borrow_mut()
            .commits
            .insert(sha.to_string(), (tree.clone(), parents.clone()));
        (tree, parents)
    }

    /// `git merge-tree --write-tree`: the merged tree, or the conflicting paths.
    fn merge_tree(&self, ours: &str, theirs: &str) -> Result<String, Conflict> {
        let out = self.raw(
            &[
                "merge-tree",
                "--write-tree",
                "--name-only",
                "--no-messages",
                ours,
                theirs,
            ],
            None,
            &[],
        );
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let mut lines = text.lines();
        let tree = lines.next().unwrap_or("").to_string();
        match out.status.code() {
            Some(0) => Ok(tree),
            Some(1) => {
                let mut files: Vec<String> = lines
                    .take_while(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect();
                files.dedup();
                Err(Conflict(files))
            }
            _ => panic!(
                "localgit: merge-tree {ours} {theirs} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            ),
        }
    }

    /// `git fsck`: whether the repository is consistent.
    pub fn fsck(&self) -> bool {
        self.raw(&["fsck", "--no-progress"], None, &[])
            .status
            .success()
    }

    /// The number of CI checkouts made so far.
    pub fn checkouts(&self) -> u64 {
        self.checkouts.get()
    }
}

impl GitStore for LocalGit {
    fn commit(&mut self, tree: Tree, parents: Vec<Sha>, message: &str) -> Sha {
        let tree_id = self.write_tree(&tree);
        self.cache.borrow_mut().trees.insert(tree_id.clone(), tree);
        self.commit_tree(&tree_id, &parents, message)
    }

    fn tree(&self, sha: &str) -> Tree {
        let tree_id = self.tree_id_of(sha);
        if let Some(t) = self.cache.borrow().trees.get(&tree_id) {
            return t.clone();
        }
        let listing = self.raw(&["ls-tree", "-r", "-z", &tree_id], None, &[]);
        let mut entries = Vec::new();
        for entry in listing.stdout.split(|b| *b == 0).filter(|e| !e.is_empty()) {
            let entry = String::from_utf8_lossy(entry);
            let Some((meta, path)) = entry.split_once('\t') else {
                continue;
            };
            let oid = meta.split_whitespace().nth(2).unwrap_or("").to_string();
            entries.push((path.to_string(), oid));
        }
        let mut tree = Tree::new();
        if !entries.is_empty() {
            let input: String = entries.iter().map(|(_, o)| format!("{o}\n")).collect();
            let out = self.raw(&["cat-file", "--batch"], Some(input.as_bytes()), &[]);
            let data = out.stdout;
            let mut pos = 0;
            for (path, _) in &entries {
                let nl = pos + data[pos..].iter().position(|b| *b == b'\n').unwrap_or(0);
                let header = String::from_utf8_lossy(&data[pos..nl]).into_owned();
                let size: usize = header
                    .split_whitespace()
                    .nth(2)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or_else(|| panic!("localgit: cat-file header `{header}`"));
                let start = nl + 1;
                let content = String::from_utf8_lossy(&data[start..start + size]).into_owned();
                tree.insert(path.clone(), content);
                pos = start + size + 1;
            }
        }
        self.cache.borrow_mut().trees.insert(tree_id, tree.clone());
        tree
    }

    fn tree_id_of(&self, sha: &str) -> String {
        self.commit_info(sha).0
    }

    fn parents(&self, sha: &str) -> Vec<Sha> {
        self.commit_info(sha).1
    }

    fn ref_sha(&self, name: &str) -> Option<Sha> {
        let out = self.raw(
            &["rev-parse", "--verify", "-q", &Self::full_ref(name)],
            None,
            &[],
        );
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn ref_names(&self) -> Vec<String> {
        self.ok(&["for-each-ref", "--format=%(refname)", "refs/heads/"])
            .lines()
            .filter_map(|l| l.strip_prefix("refs/heads/"))
            .map(str::to_string)
            .collect()
    }

    fn create_ref(&mut self, name: &str, sha: &str) -> Result<(), RefError> {
        let out = self.raw(&["update-ref", &Self::full_ref(name), sha, ZERO], None, &[]);
        if out.status.success() {
            Ok(())
        } else {
            Err(RefError::Exists(name.to_string()))
        }
    }

    fn update_ref(&mut self, name: &str, expected: &str, new: &str) -> Result<(), RefError> {
        // Compare-and-swap in git itself: the update happens under the ref's lock only if the
        // ref still holds `expected`.
        let out = self.raw(
            &["update-ref", &Self::full_ref(name), new, expected],
            None,
            &[],
        );
        if out.status.success() {
            Ok(())
        } else {
            Err(RefError::Stale {
                name: name.to_string(),
                actual: self.ref_sha(name),
            })
        }
    }

    fn delete_ref(&mut self, name: &str) -> bool {
        let Some(old) = self.ref_sha(name) else {
            return false;
        };
        self.raw(
            &["update-ref", "-d", &Self::full_ref(name), &old],
            None,
            &[],
        )
        .status
        .success()
    }

    fn is_ancestor(&self, ancestor: &str, of: &str) -> bool {
        let key = (ancestor.to_string(), of.to_string());
        if let Some(v) = self.cache.borrow().ancestor.get(&key) {
            return *v;
        }
        let out = self.raw(&["merge-base", "--is-ancestor", ancestor, of], None, &[]);
        let v = match out.status.code() {
            Some(0) => true,
            Some(1) => false,
            _ => panic!(
                "localgit: merge-base --is-ancestor: {}",
                String::from_utf8_lossy(&out.stderr)
            ),
        };
        self.cache.borrow_mut().ancestor.insert(key, v);
        v
    }

    fn merge_base(&self, a: &str, b: &str) -> Option<Sha> {
        let key = (a.to_string(), b.to_string());
        if let Some(v) = self.cache.borrow().merge_base.get(&key) {
            return v.clone();
        }
        let out = self.raw(&["merge-base", a, b], None, &[]);
        let v = out
            .status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string());
        self.cache.borrow_mut().merge_base.insert(key, v.clone());
        v
    }

    fn merge(&mut self, ours: &str, theirs: &str, message: &str) -> Result<Sha, Conflict> {
        let tree = self.merge_tree(ours, theirs)?;
        Ok(self.commit_tree(&tree, &[ours.to_string(), theirs.to_string()], message))
    }

    fn squash(&mut self, onto: &str, head: &str, message: &str) -> Result<Sha, Conflict> {
        let tree = self.merge_tree(onto, head)?;
        Ok(self.commit_tree(&tree, &[onto.to_string()], message))
    }

    fn changed_paths(&self, from: &str, to: &str) -> Vec<String> {
        let key = (from.to_string(), to.to_string());
        if let Some(v) = self.cache.borrow().changed.get(&key) {
            return v.clone();
        }
        let out = self.raw(
            &[
                "diff-tree",
                "-r",
                "-z",
                "--name-only",
                "--no-renames",
                from,
                to,
            ],
            None,
            &[],
        );
        let mut v: Vec<String> = out
            .stdout
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect();
        v.sort();
        self.cache.borrow_mut().changed.insert(key, v.clone());
        v
    }

    fn first_parent_history(&self, sha: &str) -> Vec<Sha> {
        if let Some(v) = self.cache.borrow().history.get(sha) {
            return v.clone();
        }
        let v: Vec<Sha> = self
            .ok(&["rev-list", "--first-parent", sha])
            .lines()
            .map(str::to_string)
            .collect();
        self.cache
            .borrow_mut()
            .history
            .insert(sha.to_string(), v.clone());
        v
    }

    fn run_in_checkout(&self, sha: &str, command: &str, env: &[(&str, &str)]) -> Option<i32> {
        let n = self.checkouts.get() + 1;
        self.checkouts.set(n);
        let dir = self.work.join(format!("check-{n}"));
        let dir_s = dir.to_string_lossy().into_owned();
        let added = self.raw(
            &["worktree", "add", "--detach", "-q", &dir_s, sha],
            None,
            &[],
        );
        if !added.status.success() {
            panic!(
                "localgit: checkout of {sha} failed: {}",
                String::from_utf8_lossy(&added.stderr)
            );
        }
        let code = run_shell(&dir, command, env);
        // `--force` removes this checkout even if the check left files in it; it touches no
        // ref.
        self.raw(&["worktree", "remove", "--force", &dir_s], None, &[]);
        code
    }

    fn kind(&self) -> &'static str {
        "localgit"
    }
}

/// Runs a scenario on real git: a fresh repository per run (perfect, and disturbed when the
/// scenario has faults); every check is a process in a checkout.
pub fn run(s: &Scenario) -> Result<RunReport, LocalGitError> {
    run_with(s, LocalGit::new, true)
}

/// Loads and runs a scenario file on real git.
pub fn run_file(path: &Path) -> Result<RunReport, LocalGitError> {
    let text = std::fs::read_to_string(path).map_err(|source| ScenarioError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let scenario = crate::scenario::parse(&text, path)?;
    run(&scenario)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(pairs: &[(&str, &str)]) -> Tree {
        pairs
            .iter()
            .map(|(p, c)| (p.to_string(), c.to_string()))
            .collect()
    }

    #[test]
    fn commits_trees_and_compare_and_swap() {
        let mut g = LocalGit::new().expect("git");
        let root = g.commit(
            t(&[("README.md", "x\n"), ("src/a.rs", "a\n")]),
            vec![],
            "root",
        );
        assert_eq!(
            g.tree(&root),
            t(&[("README.md", "x\n"), ("src/a.rs", "a\n")])
        );
        g.create_ref("main", &root).unwrap();
        assert!(matches!(
            g.create_ref("main", &root),
            Err(RefError::Exists(_))
        ));
        let next = g.commit(
            t(&[("README.md", "y\n"), ("src/a.rs", "a\n")]),
            vec![root.clone()],
            "next",
        );
        // Deterministic ids: the same commit again is the same object.
        assert_eq!(
            next,
            g.commit(
                t(&[("README.md", "y\n"), ("src/a.rs", "a\n")]),
                vec![root.clone()],
                "next"
            )
        );
        g.update_ref("main", &root, &next).unwrap();
        // A stale expectation is refused by git and changes nothing.
        assert_eq!(
            g.update_ref("main", &root, &root),
            Err(RefError::Stale {
                name: "main".into(),
                actual: Some(next.clone())
            })
        );
        assert_eq!(g.ref_sha("main"), Some(next.clone()));
        assert_eq!(g.changed_paths(&root, &next), vec!["README.md".to_string()]);
        assert_eq!(
            g.first_parent_history(&next),
            vec![next.clone(), root.clone()]
        );
        assert!(g.is_ancestor(&root, &next));
        assert!(!g.is_ancestor(&next, &root));
        assert!(g.delete_ref("main"));
        assert_eq!(g.ref_sha("main"), None);
        assert!(g.fsck());
    }

    #[test]
    fn line_level_merges_and_conflicts() {
        let mut g = LocalGit::new().expect("git");
        let base = "1\n2\n3\n4\n5\n6\n7\n";
        let root = g.commit(t(&[("f", base)]), vec![], "root");
        let top = g.commit(
            t(&[("f", "ONE\n2\n3\n4\n5\n6\n7\n")]),
            vec![root.clone()],
            "top",
        );
        let bottom = g.commit(
            t(&[("f", "1\n2\n3\n4\n5\n6\nSEVEN\n")]),
            vec![root.clone()],
            "bottom",
        );
        let also_top = g.commit(
            t(&[("f", "UNO\n2\n3\n4\n5\n6\n7\n")]),
            vec![root.clone()],
            "top2",
        );
        // Different lines of the same file: git merges (the in-memory model would conflict).
        let m = g.merge(&top, &bottom, "merge").expect("clean");
        assert_eq!(g.tree(&m)["f"], "ONE\n2\n3\n4\n5\n6\nSEVEN\n");
        assert_eq!(g.parents(&m), vec![top.clone(), bottom.clone()]);
        // The same line changed differently: a conflict, as git reports it.
        assert_eq!(
            g.merge(&top, &also_top, "merge"),
            Err(Conflict(vec!["f".into()]))
        );
        // Squash: the changes of `bottom` on top of `top`, one parent.
        let s = g.squash(&top, &bottom, "squash").expect("clean");
        assert_eq!(g.parents(&s), vec![top.clone()]);
        assert_eq!(g.tree_id_of(&s), g.tree_id_of(&m));
    }

    #[test]
    fn checks_run_in_a_real_checkout() {
        let mut g = LocalGit::new().expect("git");
        let root = g.commit(
            t(&[("ci/check.sh", "test -f ok\n"), ("ok", "")]),
            vec![],
            "root",
        );
        let bad = g.commit(
            t(&[("ci/check.sh", "test -f ok\n")]),
            vec![root.clone()],
            "bad",
        );
        assert_eq!(g.run_in_checkout(&root, "sh ci/check.sh", &[]), Some(0));
        assert_eq!(g.run_in_checkout(&bad, "sh ci/check.sh", &[]), Some(1));
        // The checkout is a git worktree at exactly that commit.
        assert_eq!(
            g.run_in_checkout(
                &root,
                &format!("test \"$(git rev-parse HEAD)\" = {root}"),
                &[]
            ),
            Some(0)
        );
        assert_eq!(g.checkouts(), 3);
    }
}
