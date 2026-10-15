//! Derived data: one generic mechanism for every index Wren builds.
//!
//! A changeset only records what changed, so it cannot by itself answer
//! "is A an ancestor of B" or "who last touched this line". Those answers come
//! from derived data types, each defined *recursively*: the value for a
//! changeset is computed from the changeset plus the values already derived
//! for its parents. Consequences:
//!
//! * adding a new index is one `impl Derivable`;
//! * each type can be derived eagerly at ingest or lazily on first query;
//! * values are pure functions of immutable inputs, so any server may compute
//!   and persist them concurrently without coordination (first write wins);
//!   in v0.1 that means connections/processes sharing one metadata database;
//! * clients never see these structures, so they can change freely.

use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::meta::ChangesetRow;
use crate::oid::Oid;
use crate::repo::Repo;

pub trait Derivable: Sized + Send + Sync + 'static {
    /// Stable name; part of the storage key.
    const KIND: &'static str;
    /// Whether a value depends on the parents' values. A type that depends
    /// on its changeset alone (e.g. its renames) is derived on its own,
    /// without walking to ancestors that lack a value.
    const RECURSIVE: bool = true;

    /// Compute the value for `cs` from the values of its parents (in parent
    /// order). Implementations may read other derived values of ancestors.
    fn derive(repo: &Repo, cs: &ChangesetRow, parents: &[Arc<Self>]) -> Result<Self>;

    fn encode(&self) -> Vec<u8>;
    fn decode(b: &[u8]) -> Result<Self>;
}

/// Bounded in-memory memo for hot derived values (e.g. commit-graph nodes).
#[derive(Default)]
pub struct Memo {
    map: HashMap<(&'static str, Oid), Arc<dyn Any + Send + Sync>>,
}

const MEMO_LIMIT: usize = 500_000;

impl Memo {
    fn get<D: Derivable>(&self, oid: &Oid) -> Option<Arc<D>> {
        self.map
            .get(&(D::KIND, *oid))
            .and_then(|v| v.clone().downcast::<D>().ok())
    }
    fn put<D: Derivable>(&mut self, oid: Oid, v: Arc<D>) {
        if self.map.len() >= MEMO_LIMIT {
            self.map.clear();
        }
        self.map.insert((D::KIND, oid), v);
    }
}

impl Repo {
    /// Look up an already-derived value without computing anything.
    pub fn derived_if_present<D: Derivable>(&self, oid: &Oid) -> Result<Option<Arc<D>>> {
        if let Some(v) = self.memo.lock().unwrap().get::<D>(oid) {
            return Ok(Some(v));
        }
        match self.meta.get_derived(D::KIND, oid.as_bytes())? {
            Some(b) => {
                let v = Arc::new(
                    D::decode(&b).with_context(|| format!("decoding {} for {oid}", D::KIND))?,
                );
                self.memo.lock().unwrap().put(*oid, v.clone());
                Ok(Some(v))
            }
            None => Ok(None),
        }
    }

    /// Get (deriving if necessary) the value of `D` for changeset `oid`.
    /// Ancestors that lack a value are derived first, iteratively, so deep
    /// histories do not overflow the stack.
    pub fn derived<D: Derivable>(&self, oid: &Oid) -> Result<Arc<D>> {
        if let Some(v) = self.derived_if_present::<D>(oid)? {
            return Ok(v);
        }
        if !D::RECURSIVE {
            let row = self
                .meta
                .changeset(oid)?
                .ok_or_else(|| crate::not_found(format!("unknown changeset {oid}")))?;
            let v = Arc::new(D::derive(self, &row, &[])?);
            self.meta
                .put_derived(D::KIND, oid.as_bytes(), &v.encode())?;
            self.memo.lock().unwrap().put(*oid, v.clone());
            return Ok(v);
        }
        let mut done: HashMap<Oid, Arc<D>> = HashMap::new();
        let mut stack: Vec<(Oid, Option<ChangesetRow>)> = vec![(*oid, None)];
        while let Some((cur, row)) = stack.pop() {
            if done.contains_key(&cur) {
                continue;
            }
            match row {
                None => {
                    if let Some(v) = self.derived_if_present::<D>(&cur)? {
                        done.insert(cur, v);
                        continue;
                    }
                    let row = self
                        .meta
                        .changeset(&cur)?
                        .ok_or_else(|| crate::not_found(format!("unknown changeset {cur}")))?;
                    let parents = row.parents.clone();
                    stack.push((cur, Some(row)));
                    for p in parents {
                        if !done.contains_key(&p) {
                            stack.push((p, None));
                        }
                    }
                }
                Some(row) => {
                    let parents: Vec<Arc<D>> =
                        row.parents.iter().map(|p| done[p].clone()).collect();
                    let v = Arc::new(D::derive(self, &row, &parents)?);
                    self.meta
                        .put_derived(D::KIND, cur.as_bytes(), &v.encode())?;
                    self.memo.lock().unwrap().put(cur, v.clone());
                    done.insert(cur, v);
                }
            }
        }
        Ok(done.remove(oid).unwrap())
    }
}
