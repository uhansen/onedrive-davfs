//! Id-keyed OneDrive namespace index.
//!
//! Graph delta items are keyed by item id. A move is a reparent; descendant
//! paths are derived by walking parents, never stored.

use std::collections::{BTreeSet, HashMap};

pub type Id = String;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemMeta {
    pub is_dir: bool,
    pub size: u64,
    pub mtime: u64,
    pub etag: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub parent: Option<Id>,
    pub name: String,
    pub meta: ItemMeta,
}

#[derive(Clone, Debug, Default)]
pub struct Index {
    pub nodes: HashMap<Id, Node>,
    pub kids: HashMap<Id, BTreeSet<(String, Id)>>,
    pub root: Option<Id>,
    pub delta_token: Option<String>,
    pub pending_next_link: Option<String>,
    pub crawl_complete: bool,
    pub generation: u64,
}

#[derive(Clone, Debug)]
pub struct DeltaItem {
    pub id: Id,
    pub parent_id: Option<Id>,
    pub name: String,
    pub deleted: bool,
    pub is_root: bool,
    pub meta: ItemMeta,
}

impl Index {
    pub fn apply(&mut self, it: DeltaItem) {
        if it.id.is_empty() {
            return;
        }
        if it.deleted {
            self.remove(&it.id);
            return;
        }
        if it.is_root {
            self.root = Some(it.id.clone());
            self.nodes.insert(
                it.id.clone(),
                Node {
                    parent: None,
                    name: it.name,
                    meta: it.meta,
                },
            );
            return;
        }
        let Some(parent_id) = it.parent_id.clone() else {
            return;
        };

        if let Some(old) = self.nodes.get(&it.id) {
            let moved = old.parent.as_ref() != Some(&parent_id) || old.name != it.name;
            if moved {
                if let Some(pid) = old.parent.clone() {
                    if let Some(set) = self.kids.get_mut(&pid) {
                        set.remove(&(old.name.clone(), it.id.clone()));
                    }
                }
            }
        }

        if let Some(set) = self.kids.get(&parent_id) {
            let clash = set
                .iter()
                .find(|(n, cid)| n == &it.name && cid != &it.id)
                .map(|(_, cid)| cid.clone());
            if let Some(cid) = clash {
                self.remove(&cid);
            }
        }

        self.kids
            .entry(parent_id.clone())
            .or_default()
            .insert((it.name.clone(), it.id.clone()));
        self.nodes.insert(
            it.id.clone(),
            Node {
                parent: Some(parent_id),
                name: it.name,
                meta: it.meta,
            },
        );
    }

    pub fn remove(&mut self, id: &str) {
        if let Some(node) = self.nodes.remove(id) {
            if self.root.as_deref() == Some(id) {
                self.root = None;
            }
            if let Some(pid) = &node.parent {
                if let Some(set) = self.kids.get_mut(pid) {
                    set.remove(&(node.name, id.to_string()));
                }
            }
        }
        if let Some(children) = self.kids.remove(id) {
            for (_, cid) in children {
                self.remove(&cid);
            }
        }
    }

    pub fn resolve(&self, path: &str) -> Option<Id> {
        let mut cur = self.root.clone()?;
        for seg in path.split('/').filter(|s| !s.is_empty()) {
            cur = self
                .kids
                .get(&cur)?
                .iter()
                .find(|(n, _)| n == seg)?
                .1
                .clone();
        }
        Some(cur)
    }

    #[cfg(test)]
    pub fn children_of(&self, path: &str) -> Option<Vec<(String, Id)>> {
        let id = self.resolve(path)?;
        Some(
            self.kids
                .get(&id)
                .map(|set| set.iter().cloned().collect())
                .unwrap_or_default(),
        )
    }

    pub fn path_of(&self, id: &str) -> Option<String> {
        let mut segs = Vec::new();
        let mut cur = id;
        for _ in 0..64 {
            let node = self.nodes.get(cur)?;
            match &node.parent {
                None => {
                    segs.reverse();
                    return Some(if segs.is_empty() {
                        "/".to_string()
                    } else {
                        format!("/{}", segs.join("/"))
                    });
                }
                Some(pid) => {
                    segs.push(node.name.clone());
                    cur = pid;
                }
            }
        }
        None
    }

    /// Drops nodes that cannot walk to the recorded root.
    pub fn sweep_orphans(&mut self) {
        let Some(root) = self.root.clone() else {
            return;
        };
        let reachable: Vec<Id> = self
            .nodes
            .keys()
            .filter(|id| {
                let mut cur = (*id).as_str();
                for _ in 0..64 {
                    if cur == root {
                        return true;
                    }
                    match self.nodes.get(cur).and_then(|n| n.parent.as_deref()) {
                        Some(pid) => cur = pid,
                        None => return false,
                    }
                }
                false
            })
            .cloned()
            .collect();
        let reachable: std::collections::HashSet<Id> = reachable.into_iter().collect();
        let dead: Vec<Id> = self
            .nodes
            .keys()
            .filter(|id| !reachable.contains(*id))
            .cloned()
            .collect();
        for id in dead {
            self.nodes.remove(&id);
            self.kids.remove(&id);
        }
        self.kids.retain(|pid, set| {
            if !reachable.contains(pid) {
                return false;
            }
            set.retain(|(_, cid)| reachable.contains(cid));
            true
        });
    }

    /// Insert or update a node at `path` without a Graph id (write-through
    /// fallback when the mutation response could not be parsed).
    pub fn upsert_at_path(&mut self, path: &str, meta: ItemMeta) -> bool {
        if let Some(id) = self.resolve(path) {
            if let Some(node) = self.nodes.get_mut(&id) {
                node.meta = meta;
                return true;
            }
        }
        let parent_path = parent_path(path);
        let name = last_segment(path);
        if name.is_empty() {
            return false;
        }
        let Some(parent_id) = self.resolve(&parent_path) else {
            return false;
        };
        self.apply(DeltaItem {
            id: format!("local:{path}"),
            parent_id: Some(parent_id),
            name,
            deleted: false,
            is_root: false,
            meta,
        });
        true
    }

    pub fn remove_at_path(&mut self, path: &str) -> bool {
        if let Some(id) = self.resolve(path) {
            self.remove(&id);
            true
        } else {
            false
        }
    }

    pub fn move_at_path(&mut self, from: &str, to: &str) -> bool {
        let Some(id) = self.resolve(from) else {
            return false;
        };
        let Some(node) = self.nodes.get(&id).cloned() else {
            return false;
        };
        let dest_parent = parent_path(to);
        let dest_name = last_segment(to);
        if dest_name.is_empty() {
            return false;
        }
        let Some(parent_id) = self.resolve(&dest_parent) else {
            return false;
        };
        self.apply(DeltaItem {
            id,
            parent_id: Some(parent_id),
            name: dest_name,
            deleted: false,
            is_root: false,
            meta: node.meta,
        });
        true
    }
}

pub fn parent_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(idx) => trimmed[..idx].to_string(),
    }
}

pub fn last_segment(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string()
}

pub fn normalize_path(path: &str) -> String {
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segs.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", segs.join("/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(id: &str, parent: &str, name: &str) -> DeltaItem {
        DeltaItem {
            id: id.into(),
            parent_id: Some(parent.into()),
            name: name.into(),
            deleted: false,
            is_root: false,
            meta: ItemMeta {
                is_dir: false,
                size: 1,
                mtime: 10,
                etag: "e".into(),
            },
        }
    }

    fn dir(id: &str, parent: &str, name: &str) -> DeltaItem {
        let mut it = file(id, parent, name);
        it.meta.is_dir = true;
        it
    }

    fn root(id: &str) -> DeltaItem {
        DeltaItem {
            id: id.into(),
            parent_id: None,
            name: String::new(),
            deleted: false,
            is_root: true,
            meta: ItemMeta {
                is_dir: true,
                size: 0,
                mtime: 0,
                etag: String::new(),
            },
        }
    }

    #[test]
    fn out_of_order_page_converges() {
        let mut idx = Index::default();
        idx.apply(file("c", "b", "c.txt"));
        idx.apply(dir("b", "a", "docs"));
        idx.apply(root("a"));
        assert_eq!(idx.resolve("/docs/c.txt").as_deref(), Some("c"));
        let kids = idx.children_of("/docs").unwrap();
        assert_eq!(kids, vec![("c.txt".into(), "c".into())]);
    }

    #[test]
    fn move_is_reparent_without_rewriting_descendants() {
        let mut idx = Index::default();
        idx.apply(root("r"));
        idx.apply(dir("a", "r", "a"));
        idx.apply(dir("b", "r", "b"));
        idx.apply(file("f", "a", "f.txt"));
        idx.apply(dir("a", "b", "a"));
        assert_eq!(idx.resolve("/a"), None);
        assert_eq!(idx.resolve("/b/a/f.txt").as_deref(), Some("f"));
        assert_eq!(idx.path_of("f").as_deref(), Some("/b/a/f.txt"));
    }

    #[test]
    fn delete_is_recursive() {
        let mut idx = Index::default();
        idx.apply(root("r"));
        idx.apply(dir("a", "r", "a"));
        idx.apply(file("f", "a", "f.txt"));
        idx.apply(DeltaItem {
            id: "a".into(),
            parent_id: None,
            name: String::new(),
            deleted: true,
            is_root: false,
            meta: ItemMeta {
                is_dir: true,
                size: 0,
                mtime: 0,
                etag: String::new(),
            },
        });
        assert!(idx.resolve("/a").is_none());
        assert!(idx.nodes.get("f").is_none());
    }

    #[test]
    fn rename_detaches_old_name() {
        let mut idx = Index::default();
        idx.apply(root("r"));
        idx.apply(file("f", "r", "old.txt"));
        idx.apply(file("f", "r", "new.txt"));
        assert!(idx.resolve("/old.txt").is_none());
        assert_eq!(idx.resolve("/new.txt").as_deref(), Some("f"));
    }

    #[test]
    fn same_name_replaces_other_id() {
        let mut idx = Index::default();
        idx.apply(root("r"));
        idx.apply(file("local:/x", "r", "x.txt"));
        idx.apply(file("real", "r", "x.txt"));
        assert_eq!(idx.resolve("/x.txt").as_deref(), Some("real"));
        assert!(idx.nodes.get("local:/x").is_none());
    }

    #[test]
    fn orphan_sweep_drops_unreachable() {
        let mut idx = Index::default();
        idx.apply(root("r"));
        idx.apply(file("f", "missing", "f.txt"));
        idx.sweep_orphans();
        assert!(idx.nodes.get("f").is_none());
        assert!(idx.nodes.get("r").is_some());
    }

    #[test]
    fn upsert_and_remove_by_path() {
        let mut idx = Index::default();
        idx.apply(root("r"));
        assert!(idx.upsert_at_path(
            "/n.txt",
            ItemMeta {
                is_dir: false,
                size: 4,
                mtime: 1,
                etag: String::new(),
            }
        ));
        assert!(idx.resolve("/n.txt").is_some());
        assert!(idx.remove_at_path("/n.txt"));
        assert!(idx.resolve("/n.txt").is_none());
    }
}
