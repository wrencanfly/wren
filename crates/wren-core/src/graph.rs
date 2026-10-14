//! Commit graph as derived data: generation numbers plus a first-parent
//! skip list. Together they answer ancestry, merge-base and "when did this
//! land on main" without walking the whole log.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::sync::Arc;

use anyhow::{bail, Result};

use crate::changes::Reader;
use crate::derived::Derivable;
use crate::meta::ChangesetRow;
use crate::oid::Oid;
use crate::repo::Repo;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphNode {
    /// 1 for root commits, otherwise 1 + max(parent generation). Any ancestor
    /// has a strictly smaller generation.
    pub generation: u32,
    /// Number of first-parent steps to a root.
    pub depth: u32,
    pub parents: Vec<Oid>,
    /// `skip[k]` is the first-parent ancestor 2^k steps back.
    pub skip: Vec<Oid>,
}

impl Derivable for GraphNode {
    const KIND: &'static str = "graph";

    fn derive(repo: &Repo, cs: &ChangesetRow, parents: &[Arc<Self>]) -> Result<Self> {
        let generation = 1 + parents.iter().map(|p| p.generation).max().unwrap_or(0);
        let depth = parents.first().map(|p| p.depth + 1).unwrap_or(0);
        let mut skip = Vec::new();
        if let Some(p1) = cs.parents.first() {
            skip.push(*p1);
            let mut node = parents[0].clone();
            // skip[k+1] = skip[k] of the node at skip[k].
            while let Some(&next) = node.skip.get(skip.len() - 1) {
                skip.push(next);
                node = repo.derived::<GraphNode>(&next)?;
            }
        }
        Ok(GraphNode {
            generation,
            depth,
            parents: cs.parents.clone(),
            skip,
        })
    }

    fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(10 + 20 * (self.parents.len() + self.skip.len()));
        b.extend(self.generation.to_le_bytes());
        b.extend(self.depth.to_le_bytes());
        b.push(self.parents.len() as u8);
        self.parents.iter().for_each(|p| b.extend(p.0));
        b.push(self.skip.len() as u8);
        self.skip.iter().for_each(|p| b.extend(p.0));
        b
    }

    fn decode(b: &[u8]) -> Result<Self> {
        let mut r = Reader { b, i: 0 };
        let generation = r.u32()?;
        let depth = r.u32()?;
        let np = r.take(1)?[0] as usize;
        let parents = (0..np).map(|_| r.oid()).collect::<Result<_>>()?;
        let ns = r.take(1)?[0] as usize;
        let skip = (0..ns).map(|_| r.oid()).collect::<Result<_>>()?;
        Ok(GraphNode {
            generation,
            depth,
            parents,
            skip,
        })
    }
}

impl Repo {
    pub fn node(&self, oid: &Oid) -> Result<Arc<GraphNode>> {
        self.derived::<GraphNode>(oid)
    }

    /// The first-parent ancestor of `oid` at first-parent depth `depth`.
    pub fn first_parent_at_depth(&self, oid: &Oid, depth: u32) -> Result<Option<Oid>> {
        let node_depth = self.node(oid)?.depth;
        if depth > node_depth {
            return Ok(None);
        }
        self.first_parent_steps(*oid, node_depth - depth).map(Some)
    }

    /// Walk `steps` first-parent steps back from `oid` using the skip list:
    /// O(log steps) node lookups.
    pub fn first_parent_steps(&self, oid: Oid, mut steps: u32) -> Result<Oid> {
        let mut cur = oid;
        while steps > 0 {
            let node = self.node(&cur)?;
            // A node at depth d has skip[k] for every 2^k <= d, and steps <= d,
            // so the highest set bit of `steps` is always available.
            let k = (31 - steps.leading_zeros()) as usize;
            let Some(next) = node.skip.get(k) else {
                bail!("walked past a root commit")
            };
            cur = *next;
            steps -= 1 << k;
        }
        Ok(cur)
    }

    /// Is `c` on the first-parent chain of `tip` (including `tip` itself)?
    pub fn on_first_parent_chain(&self, tip: &Oid, c: &Oid) -> Result<bool> {
        let depth = self.node(c)?.depth;
        Ok(self.first_parent_at_depth(tip, depth)? == Some(*c))
    }

    /// Is `a` an ancestor of `b`? (A commit counts as its own ancestor.)
    pub fn is_ancestor(&self, a: &Oid, b: &Oid) -> Result<bool> {
        if a == b {
            return Ok(true);
        }
        let (na, nb) = (self.node(a)?, self.node(b)?);
        if na.generation >= nb.generation {
            return Ok(false);
        }
        // Fast path: a is on b's first-parent chain.
        if na.depth < nb.depth && self.first_parent_at_depth(b, na.depth)? == Some(*a) {
            return Ok(true);
        }
        // Generation-pruned search.
        let mut seen = HashSet::new();
        let mut stack = vec![*b];
        while let Some(c) = stack.pop() {
            for p in &self.node(&c)?.parents {
                if p == a {
                    return Ok(true);
                }
                if self.node(p)?.generation > na.generation && seen.insert(*p) {
                    stack.push(*p);
                }
            }
        }
        Ok(false)
    }

    /// All best common ancestors of `a` and `b` (git merge-base --all).
    ///
    /// Commits are visited in decreasing generation order, which is a
    /// topological order, so once a commit is popped its flags are final and
    /// any common ancestor of an earlier result is already marked stale.
    pub fn merge_bases(&self, a: &Oid, b: &Oid) -> Result<Vec<Oid>> {
        const P1: u8 = 1;
        const P2: u8 = 2;
        const STALE: u8 = 4;
        if a == b {
            return Ok(vec![*a]);
        }
        let mut flags: HashMap<Oid, u8> = HashMap::new();
        let mut heap: BinaryHeap<(u32, Reverse<Oid>)> = BinaryHeap::new();
        let mut queued_nonstale = 0usize;
        for (oid, f) in [(*a, P1), (*b, P2)] {
            flags.insert(oid, f);
            heap.push((self.node(&oid)?.generation, Reverse(oid)));
            queued_nonstale += 1;
        }
        let mut results = Vec::new();
        let mut popped = HashSet::new();
        while queued_nonstale > 0 {
            let Some((_, Reverse(c))) = heap.pop() else {
                break;
            };
            if !popped.insert(c) {
                continue;
            }
            let mut f = flags[&c];
            if f & STALE == 0 {
                queued_nonstale -= 1;
            }
            if f & (P1 | P2) == (P1 | P2) && f & STALE == 0 {
                results.push(c);
                f |= STALE;
                flags.insert(c, f);
            }
            for p in self.node(&c)?.parents.iter() {
                let pf = flags.get(p).copied();
                let merged = pf.unwrap_or(0) | f;
                if pf == Some(merged) {
                    continue;
                }
                let was_nonstale_queued = pf.is_some_and(|x| x & STALE == 0) && !popped.contains(p);
                flags.insert(*p, merged);
                if pf.is_none() {
                    heap.push((self.node(p)?.generation, Reverse(*p)));
                    if merged & STALE == 0 {
                        queued_nonstale += 1;
                    }
                } else if was_nonstale_queued && merged & STALE != 0 {
                    queued_nonstale -= 1;
                }
            }
        }
        results.sort();
        Ok(results)
    }

    /// The first commit on `tip`'s first-parent chain that contains `c`
    /// (for `tip = main`: the commit that landed `c` on main). Binary lifting
    /// over the skip list: O(log n) ancestry checks instead of scanning the log.
    pub fn landed(&self, c: &Oid, tip: &Oid) -> Result<Option<Oid>> {
        if !self.is_ancestor(c, tip)? {
            return Ok(None);
        }
        let mut cur = *tip;
        let mut k = self.node(&cur)?.skip.len();
        while k > 0 {
            k -= 1;
            let node = self.node(&cur)?;
            if let Some(cand) = node.skip.get(k) {
                if self.is_ancestor(c, cand)? {
                    cur = *cand;
                }
            }
        }
        Ok(Some(cur))
    }

    /// The newest commit on `tip`'s first-parent chain that is an ancestor
    /// of (or equal to) `c`, if any. Being an ancestor of `c` is monotone
    /// along the chain, so binary lifting over the skip list finds the
    /// boundary in O(log n) ancestry checks.
    pub fn newest_chain_ancestor(&self, tip: &Oid, c: &Oid) -> Result<Option<Oid>> {
        if self.is_ancestor(tip, c)? {
            return Ok(Some(*tip));
        }
        // Walk back as far as possible while staying on commits that are NOT
        // ancestors of `c`; the first-parent of where we stop is the answer.
        let mut cur = *tip;
        let mut k = self.node(&cur)?.skip.len();
        while k > 0 {
            k -= 1;
            if let Some(cand) = self.node(&cur)?.skip.get(k).copied() {
                if !self.is_ancestor(&cand, c)? {
                    cur = cand;
                }
            }
        }
        Ok(self.node(&cur)?.parents.first().copied())
    }

    /// One step of first-parent bisection between a known-good and a
    /// known-bad commit. The search runs over `bad`'s first-parent history.
    /// Its lower bound is the newest commit on that history that is an
    /// ancestor of `good`; like git bisect, it assumes ancestors of a good
    /// commit are good. (`good` itself need not be on the history: a commit
    /// on a side branch only vouches for what it contains.) On main this
    /// finds the first bad *landing*, usually a merge. Each step is O(log n)
    /// graph lookups: no log scan, no checkout.
    pub fn bisect_step(&self, good: &Oid, bad: &Oid) -> Result<crate::api::BisectStep> {
        let Some(base) = self.newest_chain_ancestor(bad, good)? else {
            return Err(crate::bad_request(format!(
                "good commit {good} shares no history with the first-parent history of {bad}"
            )));
        };
        if base == *bad {
            return Err(crate::bad_request(format!(
                "bad commit {bad} is an ancestor of good commit {good}"
            )));
        }
        let (lo, hi) = (self.node(&base)?.depth, self.node(bad)?.depth);
        let candidates = hi - lo - 1;
        if candidates == 0 {
            return Ok(crate::api::BisectStep {
                next: None,
                first_bad: Some(*bad),
                candidates,
                steps_left: 0,
            });
        }
        let mid = self
            .first_parent_at_depth(bad, lo + (hi - lo) / 2)?
            .expect("depth within range");
        let steps_left = 32 - candidates.leading_zeros();
        Ok(crate::api::BisectStep {
            next: Some(mid),
            first_bad: None,
            candidates,
            steps_left,
        })
    }
}
