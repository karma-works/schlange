//! Git as the forge sees it. [`GitStore`] is the interface the forge logic (`sim`) and the
//! invariant checker use; two stores implement it:
//!
//! - [`Repo`], a small content-addressed model in memory: commits with trees (path → content)
//!   and parents, deterministic ids, refs updated as compare-and-swap, a three-way merge at
//!   file level and squash commits. A file conflicts when both sides changed it differently
//!   relative to the merge base. There is no line-level merge: two changes to the same file
//!   always conflict unless they are equal.
//! - `localgit::LocalGit` (feature `localgit`): real git repositories on disk, with git's own
//!   line-level three-way merges and `git update-ref <ref> <new> <old>` as compare-and-swap.

use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest, Sha256};

pub type Sha = String;
pub type Tree = BTreeMap<String, String>;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Content id of a tree.
pub fn tree_id(tree: &Tree) -> String {
    let mut h = Sha256::new();
    for (path, content) in tree {
        h.update(path.as_bytes());
        h.update([0]);
        h.update(content.as_bytes());
        h.update([0]);
    }
    hex(&h.finalize())[..40].to_string()
}

#[derive(Clone, Debug)]
pub struct Commit {
    pub tree: Tree,
    pub tree_id: String,
    pub parents: Vec<Sha>,
    pub message: String,
    /// This commit and every ancestor.
    pub ancestors: BTreeSet<Sha>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RefError {
    #[error("ref `{0}` already exists")]
    Exists(String),
    #[error("ref `{name}` is at {actual:?}, not at the expected commit")]
    Stale { name: String, actual: Option<Sha> },
}

/// Files that conflict in a merge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict(pub Vec<String>);

/// The git operations the forge needs. Commits are immutable, so reads take `&self`; ref
/// updates are compare-and-swap and never forced.
pub trait GitStore {
    /// Creates (or finds) a commit with exactly this tree.
    fn commit(&mut self, tree: Tree, parents: Vec<Sha>, message: &str) -> Sha;
    /// The full tree of a commit (path → content).
    fn tree(&self, sha: &str) -> Tree;
    /// The id of a commit's tree.
    fn tree_id_of(&self, sha: &str) -> String;
    fn parents(&self, sha: &str) -> Vec<Sha>;
    fn ref_sha(&self, name: &str) -> Option<Sha>;
    /// All ref (branch) names.
    fn ref_names(&self) -> Vec<String>;
    fn create_ref(&mut self, name: &str, sha: &str) -> Result<(), RefError>;
    /// Compare-and-swap: moves `name` from `expected` to `new`, never forced.
    fn update_ref(&mut self, name: &str, expected: &str, new: &str) -> Result<(), RefError>;
    fn delete_ref(&mut self, name: &str) -> bool;
    fn is_ancestor(&self, ancestor: &str, of: &str) -> bool;
    fn merge_base(&self, a: &str, b: &str) -> Option<Sha>;
    /// A merge commit of `theirs` into `ours` (two parents).
    fn merge(&mut self, ours: &str, theirs: &str, message: &str) -> Result<Sha, Conflict>;
    /// The changes of `head` since its merge base with `onto`, as one commit whose single
    /// parent is `onto`.
    fn squash(&mut self, onto: &str, head: &str, message: &str) -> Result<Sha, Conflict>;
    /// Replays one commit onto `onto` (the change from its first parent), as a commit whose
    /// single parent is `onto` (`rebase` landing).
    fn cherry_pick(&mut self, onto: &str, commit: &str, message: &str) -> Result<Sha, Conflict>;
    /// The commit message.
    fn message(&self, sha: &str) -> String;
    /// Paths that differ between two commits' trees.
    fn changed_paths(&self, from: &str, to: &str) -> Vec<String>;
    /// First-parent history of a commit, newest first.
    fn first_parent_history(&self, sha: &str) -> Vec<Sha>;
    /// Runs `sh -c command` in a checkout of `sha` and returns its exit code (`None` when it
    /// could not start or was killed by a signal). The default writes the tree into a fresh
    /// temporary directory.
    fn run_in_checkout(&self, sha: &str, command: &str, env: &[(&str, &str)]) -> Option<i32> {
        crate::ci::run_in_tree(&self.tree(sha), command, env)
    }
    /// A short name for reports: `sim` or `localgit`.
    fn kind(&self) -> &'static str;
}

impl GitStore for Repo {
    fn commit(&mut self, tree: Tree, parents: Vec<Sha>, message: &str) -> Sha {
        Repo::commit(self, tree, parents, message)
    }
    fn tree(&self, sha: &str) -> Tree {
        Repo::tree(self, sha).clone()
    }
    fn tree_id_of(&self, sha: &str) -> String {
        Repo::tree_id_of(self, sha)
    }
    fn parents(&self, sha: &str) -> Vec<Sha> {
        self.get(sha).parents.clone()
    }
    fn ref_sha(&self, name: &str) -> Option<Sha> {
        Repo::ref_sha(self, name).cloned()
    }
    fn ref_names(&self) -> Vec<String> {
        self.refs.keys().cloned().collect()
    }
    fn create_ref(&mut self, name: &str, sha: &str) -> Result<(), RefError> {
        Repo::create_ref(self, name, sha)
    }
    fn update_ref(&mut self, name: &str, expected: &str, new: &str) -> Result<(), RefError> {
        Repo::update_ref(self, name, expected, new)
    }
    fn delete_ref(&mut self, name: &str) -> bool {
        Repo::delete_ref(self, name)
    }
    fn is_ancestor(&self, ancestor: &str, of: &str) -> bool {
        Repo::is_ancestor(self, ancestor, of)
    }
    fn merge_base(&self, a: &str, b: &str) -> Option<Sha> {
        Repo::merge_base(self, a, b)
    }
    fn merge(&mut self, ours: &str, theirs: &str, message: &str) -> Result<Sha, Conflict> {
        Repo::merge(self, ours, theirs, message)
    }
    fn squash(&mut self, onto: &str, head: &str, message: &str) -> Result<Sha, Conflict> {
        Repo::squash(self, onto, head, message)
    }
    fn cherry_pick(&mut self, onto: &str, commit: &str, message: &str) -> Result<Sha, Conflict> {
        let c = self.get(commit).clone();
        let base = c
            .parents
            .first()
            .map(|p| self.tree(p).clone())
            .unwrap_or_default();
        let tree = Repo::merge_trees(&base, self.tree(onto), &c.tree)?;
        Ok(Repo::commit(self, tree, vec![onto.to_string()], message))
    }
    fn message(&self, sha: &str) -> String {
        self.get(sha).message.clone()
    }
    fn changed_paths(&self, from: &str, to: &str) -> Vec<String> {
        Repo::changed_paths(self, from, to)
    }
    fn first_parent_history(&self, sha: &str) -> Vec<Sha> {
        Repo::first_parent_history(self, sha)
    }
    fn kind(&self) -> &'static str {
        "sim"
    }
}

#[derive(Clone, Debug, Default)]
pub struct Repo {
    commits: BTreeMap<Sha, Commit>,
    refs: BTreeMap<String, Sha>,
}

impl Repo {
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates (or finds) the commit; the id depends on tree, parents and message only.
    pub fn commit(&mut self, tree: Tree, parents: Vec<Sha>, message: &str) -> Sha {
        let tid = tree_id(&tree);
        let mut h = Sha256::new();
        h.update(tid.as_bytes());
        for p in &parents {
            h.update(b"\nparent ");
            h.update(p.as_bytes());
        }
        h.update(b"\n\n");
        h.update(message.as_bytes());
        let sha = hex(&h.finalize())[..40].to_string();
        if !self.commits.contains_key(&sha) {
            let mut ancestors: BTreeSet<Sha> = BTreeSet::new();
            for p in &parents {
                ancestors.extend(self.ancestors(p).iter().cloned());
            }
            ancestors.insert(sha.clone());
            self.commits.insert(
                sha.clone(),
                Commit {
                    tree,
                    tree_id: tid,
                    parents,
                    message: message.to_string(),
                    ancestors,
                },
            );
        }
        sha
    }

    pub fn get(&self, sha: &str) -> &Commit {
        self.commits
            .get(sha)
            .unwrap_or_else(|| panic!("unknown commit {sha}"))
    }

    /// The commit, or `None` when the id is unknown.
    pub fn get_opt(&self, sha: &str) -> Option<&Commit> {
        self.commits.get(sha)
    }

    pub fn tree(&self, sha: &str) -> &Tree {
        &self.get(sha).tree
    }

    pub fn tree_id_of(&self, sha: &str) -> String {
        self.get(sha).tree_id.clone()
    }

    pub fn ref_sha(&self, name: &str) -> Option<&Sha> {
        self.refs.get(name)
    }

    pub fn refs(&self) -> &BTreeMap<String, Sha> {
        &self.refs
    }

    pub fn create_ref(&mut self, name: &str, sha: &str) -> Result<(), RefError> {
        if self.refs.contains_key(name) {
            return Err(RefError::Exists(name.to_string()));
        }
        self.refs.insert(name.to_string(), sha.to_string());
        Ok(())
    }

    /// Compare-and-swap: moves `name` from `expected` to `new`, never forced.
    pub fn update_ref(&mut self, name: &str, expected: &str, new: &str) -> Result<(), RefError> {
        match self.refs.get(name) {
            Some(actual) if actual == expected => {
                self.refs.insert(name.to_string(), new.to_string());
                Ok(())
            }
            actual => Err(RefError::Stale {
                name: name.to_string(),
                actual: actual.cloned(),
            }),
        }
    }

    pub fn delete_ref(&mut self, name: &str) -> bool {
        self.refs.remove(name).is_some()
    }

    /// The commit and all its ancestors (computed once per commit, at creation).
    pub fn ancestors(&self, sha: &str) -> &BTreeSet<Sha> {
        &self.get(sha).ancestors
    }

    pub fn is_ancestor(&self, ancestor: &str, of: &str) -> bool {
        self.ancestors(of).contains(ancestor)
    }

    /// A best common ancestor: one that is not an ancestor of another common ancestor. Ties
    /// are broken by id, so the result is deterministic.
    pub fn merge_base(&self, a: &str, b: &str) -> Option<Sha> {
        let common: BTreeSet<&Sha> = self.ancestors(a).intersection(self.ancestors(b)).collect();
        // Proper ancestors of common ancestors are not best.
        let mut dominated: BTreeSet<&Sha> = BTreeSet::new();
        for c in &common {
            for p in &self.get(c).parents {
                dominated.extend(self.ancestors(p).iter());
            }
        }
        common
            .into_iter()
            .find(|c| !dominated.contains(*c))
            .map(|s| s.to_string())
    }

    /// Three-way merge of trees at file level.
    pub fn merge_trees(base: &Tree, ours: &Tree, theirs: &Tree) -> Result<Tree, Conflict> {
        let paths: BTreeSet<&String> = base
            .keys()
            .chain(ours.keys())
            .chain(theirs.keys())
            .collect();
        let mut out = Tree::new();
        let mut conflicts = Vec::new();
        for p in paths {
            let (b, o, t) = (base.get(p), ours.get(p), theirs.get(p));
            let merged = if o == t || t == b {
                o
            } else if o == b {
                t
            } else {
                conflicts.push(p.clone());
                continue;
            };
            if let Some(content) = merged {
                out.insert(p.clone(), content.clone());
            }
        }
        if conflicts.is_empty() {
            Ok(out)
        } else {
            Err(Conflict(conflicts))
        }
    }

    fn merged_tree(&self, ours: &str, theirs: &str) -> Result<Tree, Conflict> {
        let base = self
            .merge_base(ours, theirs)
            .map(|b| self.tree(&b).clone())
            .unwrap_or_default();
        Self::merge_trees(&base, self.tree(ours), self.tree(theirs))
    }

    /// A merge commit of `theirs` into `ours` (two parents).
    pub fn merge(&mut self, ours: &str, theirs: &str, message: &str) -> Result<Sha, Conflict> {
        let tree = self.merged_tree(ours, theirs)?;
        Ok(self.commit(tree, vec![ours.to_string(), theirs.to_string()], message))
    }

    /// A squash commit: the changes of `head` since its merge base with `onto`, as one commit
    /// with the single parent `onto`.
    pub fn squash(&mut self, onto: &str, head: &str, message: &str) -> Result<Sha, Conflict> {
        let tree = self.merged_tree(onto, head)?;
        Ok(self.commit(tree, vec![onto.to_string()], message))
    }

    /// Paths that differ between two commits' trees.
    pub fn changed_paths(&self, from: &str, to: &str) -> Vec<String> {
        let (a, b) = (self.tree(from), self.tree(to));
        let paths: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
        paths
            .into_iter()
            .filter(|p| a.get(*p) != b.get(*p))
            .cloned()
            .collect()
    }

    /// First-parent history of a commit, newest first.
    pub fn first_parent_history(&self, sha: &str) -> Vec<Sha> {
        let mut out = vec![sha.to_string()];
        let mut cur = sha.to_string();
        while let Some(p) = self.get(&cur).parents.first() {
            out.push(p.clone());
            cur = p.clone();
        }
        out
    }
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
    fn ids_are_deterministic() {
        let mut a = Repo::new();
        let mut b = Repo::new();
        let x = a.commit(t(&[("f", "1")]), vec![], "m");
        let y = b.commit(t(&[("f", "1")]), vec![], "m");
        assert_eq!(x, y);
        assert_ne!(x, a.commit(t(&[("f", "2")]), vec![], "m"));
    }

    #[test]
    fn three_way_merge_and_conflicts() {
        let base = t(&[("a", "0"), ("b", "0")]);
        let ours = t(&[("a", "1"), ("b", "0")]);
        let theirs = t(&[("a", "0"), ("b", "2"), ("c", "new")]);
        assert_eq!(
            Repo::merge_trees(&base, &ours, &theirs).unwrap(),
            t(&[("a", "1"), ("b", "2"), ("c", "new")])
        );
        let clash = t(&[("a", "9"), ("b", "0")]);
        assert_eq!(
            Repo::merge_trees(&base, &ours, &clash),
            Err(Conflict(vec!["a".into()]))
        );
        // Same change on both sides is no conflict; a deletion on one side wins.
        assert!(Repo::merge_trees(&base, &ours, &ours).is_ok());
        let deleted = t(&[("a", "0")]);
        assert_eq!(
            Repo::merge_trees(&base, &base, &deleted).unwrap(),
            t(&[("a", "0")])
        );
    }

    #[test]
    fn squash_and_cas() {
        let mut r = Repo::new();
        let root = r.commit(t(&[("readme", "x")]), vec![], "root");
        r.create_ref("main", &root).unwrap();
        let pr = r.commit(t(&[("readme", "x"), ("f", "pr")]), vec![root.clone()], "pr");
        let other = r.commit(t(&[("readme", "y")]), vec![root.clone()], "direct");
        r.update_ref("main", &root, &other).unwrap();
        // CAS with a stale expectation fails and changes nothing.
        assert!(matches!(
            r.update_ref("main", &root, &pr),
            Err(RefError::Stale { .. })
        ));
        let sq = r.squash(&other, &pr, "squash").unwrap();
        assert_eq!(r.get(&sq).parents, vec![other.clone()]);
        assert_eq!(r.tree(&sq), &t(&[("readme", "y"), ("f", "pr")]));
        assert_eq!(r.merge_base(&sq, &pr), Some(root.clone()));
        assert!(r.is_ancestor(&root, &sq));
        assert_eq!(r.first_parent_history(&sq), vec![sq.clone(), other, root]);
    }
}
