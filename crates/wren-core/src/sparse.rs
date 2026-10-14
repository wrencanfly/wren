//! Sparse checkout specification with git "cone mode" semantics: a set of
//! directories checked out recursively, plus the files directly inside the
//! root and inside every ancestor of those directories. Matching git exactly
//! lets a Wren checkout double as a valid sparse git working tree.

use crate::changes::normalize_path;

#[derive(Clone, Debug, Default)]
pub struct Sparse {
    /// Normalised, deduplicated cone directories. Empty = full checkout.
    dirs: Vec<String>,
}

impl Sparse {
    pub fn new<S: AsRef<str>>(dirs: &[S]) -> Sparse {
        let mut v: Vec<String> = dirs.iter().map(|d| normalize_path(d.as_ref())).collect();
        if v.iter().any(|d| d.is_empty()) {
            return Sparse::default();
        }
        v.sort();
        v.dedup();
        // Drop directories nested inside another cone directory.
        let mut dirs: Vec<String> = Vec::new();
        for d in v {
            if !dirs.iter().any(|p| is_within(&d, p)) {
                dirs.push(d);
            }
        }
        Sparse { dirs }
    }

    pub fn is_full(&self) -> bool {
        self.dirs.is_empty()
    }

    pub fn dirs(&self) -> &[String] {
        &self.dirs
    }

    /// Must the client descend into directory `path`?
    pub fn wants_dir(&self, path: &str) -> bool {
        self.is_full()
            || self
                .dirs
                .iter()
                .any(|d| is_within(path, d) || is_within(d, path))
    }

    /// Is the file at `path` part of the checkout?
    pub fn wants_file(&self, path: &str) -> bool {
        if self.is_full() {
            return true;
        }
        let parent = match path.rfind('/') {
            Some(i) => &path[..i],
            None => return true, // root-level files are always included
        };
        self.dirs
            .iter()
            .any(|d| is_within(parent, d) || is_within(d, parent))
    }

    /// Contents for `.git/info/sparse-checkout` in cone mode.
    pub fn git_patterns(&self) -> String {
        let mut out = String::from("/*\n!/*/\n");
        let mut parents: Vec<&str> = Vec::new();
        for d in &self.dirs {
            let mut p = d.as_str();
            while let Some(i) = p.rfind('/') {
                p = &p[..i];
                parents.push(p);
            }
        }
        parents.sort();
        parents.dedup();
        for p in parents {
            out.push_str(&format!("/{p}/\n!/{p}/*/\n"));
        }
        for d in &self.dirs {
            out.push_str(&format!("/{d}/\n"));
        }
        out
    }
}

/// Is `path` equal to `dir` or inside it?
fn is_within(path: &str, dir: &str) -> bool {
    path == dir
        || (path.len() > dir.len() && path.starts_with(dir) && path.as_bytes()[dir.len()] == b'/')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cone_semantics() {
        let s = Sparse::new(&["a/b", "a/b/c", "/x/"]);
        assert_eq!(s.dirs(), &["a/b".to_string(), "x".to_string()]);
        assert!(s.wants_dir("a"));
        assert!(s.wants_dir("a/b"));
        assert!(s.wants_dir("a/b/c/d"));
        assert!(!s.wants_dir("a/c"));
        assert!(!s.wants_dir("ab"));
        assert!(s.wants_file("README"));
        assert!(s.wants_file("a/top.txt"));
        assert!(s.wants_file("a/b/deep/f"));
        assert!(!s.wants_file("a/c/f"));
        assert!(!s.wants_file("y/f"));
        assert_eq!(s.git_patterns(), "/*\n!/*/\n/a/\n!/a/*/\n/a/b/\n/x/\n");
    }
}
