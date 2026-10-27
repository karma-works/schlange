//! A small content-addressed git model for the simulated forge: commits with trees (path →
//! content) and parents, deterministic ids, refs updated as compare-and-swap, a three-way merge
//! at file level and squash commits.
//!
//! A file conflicts when both sides changed it differently relative to the merge base. There
//! is no line-level merge: two changes to the same file always conflict unless they are equal.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

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
        self.commits.entry(sha.clone()).or_insert(Commit {
            tree,
            tree_id: tid,
            parents,
            message: message.to_string(),
        });
        sha
    }

    pub fn get(&self, sha: &str) -> &Commit {
        self.commits
            .get(sha)
            .unwrap_or_else(|| panic!("unknown commit {sha}"))
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

    /// The commit and all its ancestors.
    pub fn ancestors(&self, sha: &str) -> BTreeSet<Sha> {
        let mut seen = BTreeSet::new();
        let mut todo = VecDeque::from([sha.to_string()]);
        while let Some(s) = todo.pop_front() {
            if seen.insert(s.clone()) {
                todo.extend(self.get(&s).parents.iter().cloned());
            }
        }
        seen
    }

    pub fn is_ancestor(&self, ancestor: &str, of: &str) -> bool {
        self.ancestors(of).contains(ancestor)
    }

    /// A best common ancestor: one that is not an ancestor of another common ancestor. Ties
    /// are broken by id, so the result is deterministic.
    pub fn merge_base(&self, a: &str, b: &str) -> Option<Sha> {
        let common: BTreeSet<Sha> = self
            .ancestors(a)
            .intersection(&self.ancestors(b))
            .cloned()
            .collect();
        let mut best: Vec<&Sha> = common
            .iter()
            .filter(|c| {
                !common
                    .iter()
                    .any(|o| o != *c && self.ancestors(o).contains(*c))
            })
            .collect();
        best.sort();
        best.first().map(|s| s.to_string())
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
