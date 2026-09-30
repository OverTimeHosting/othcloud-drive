//! The in-memory namespace of one mounted drive: the remote `tree` (flat, by
//! id) turned into a path tree, plus the local-only state layered on top
//! (files not uploaded yet, dirty files, temp files, "shadow" renames).
//!
//! Nodes are addressed by a local `NodeId` that never changes for the life
//! of the mount, so an open handle survives renames and the id a file gets
//! after its first upload.

use crate::api::{RemoteFile, RemoteFolder, Tree};
use crate::util::{display_name, fold, iso_to_filetime, now_filetime, split_path};
use std::collections::{HashMap, HashSet};

pub type NodeId = u64;
pub const ROOT: NodeId = 1;

#[derive(Debug, Clone)]
pub struct Node {
    pub id: NodeId,
    pub is_dir: bool,
    /// Name as shown locally (case preserved).
    pub name: String,
    pub folded: String,
    pub parent: NodeId,
    /// Server id (`file_…` / `fold_…`). None = exists only locally so far.
    pub remote_id: Option<String>,
    /// Last known server placement. Equal to (name, parent's remote id)
    /// except while `shadow`.
    pub remote_name: String,
    pub remote_parent: Option<String>,
    /// Renamed locally to a scratch name (Office's "rename original to
    /// ~WRL0001.tmp" step); the server still has it under `remote_name`.
    pub shadow: bool,
    pub size: u64,
    pub created: u64,
    pub modified: u64,
    /// "<size>|<updatedAt>" of the server version the cache holds.
    pub remote_tag: String,
    /// Local bytes differ from the server: needs an upload.
    pub dirty: bool,
    /// Bumped on every local content change; an upload only clears `dirty`
    /// if the generation it read is still current.
    pub gen: u64,
    /// Cache file key (files only).
    pub cache_key: String,
    pub open_count: u32,
}

impl Node {
    pub fn is_file(&self) -> bool {
        !self.is_dir
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Added { path: String, dir: bool, rid: String },
    Removed { path: String, dir: bool, rid: String },
    Modified { path: String, rid: String },
    Renamed { from: String, to: String, dir: bool, rid: String },
}

#[derive(Debug, Default)]
pub struct ApplyResult {
    pub changes: Vec<Change>,
    /// Cache keys whose server content changed (drop the cached bytes).
    pub invalidated: Vec<String>,
    /// Cache keys of files that no longer exist.
    pub removed: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookup {
    Found(NodeId),
    /// The last component is missing but its parent directory exists.
    NotFound,
    /// Some intermediate component is missing or is a file.
    PathNotFound,
}

pub fn remote_tag(size: u64, updated_at: Option<&str>) -> String {
    format!("{size}|{}", updated_at.unwrap_or(""))
}

pub struct Namespace {
    nodes: HashMap<NodeId, Node>,
    children: HashMap<NodeId, HashMap<String, Vec<NodeId>>>,
    by_remote: HashMap<String, NodeId>,
    next: NodeId,
    local_seq: u64,
}

fn rkey(is_dir: bool, rid: &str) -> String {
    format!("{}:{rid}", if is_dir { 'd' } else { 'f' })
}

impl Default for Namespace {
    fn default() -> Self {
        Self::new()
    }
}

impl Namespace {
    pub fn new() -> Self {
        let now = now_filetime();
        let root = Node {
            id: ROOT,
            is_dir: true,
            name: String::new(),
            folded: String::new(),
            parent: ROOT,
            remote_id: None,
            remote_name: String::new(),
            remote_parent: None,
            shadow: false,
            size: 0,
            created: now,
            modified: now,
            remote_tag: String::new(),
            dirty: false,
            gen: 0,
            cache_key: String::new(),
            open_count: 0,
        };
        let mut nodes = HashMap::new();
        nodes.insert(ROOT, root);
        let mut children = HashMap::new();
        children.insert(ROOT, HashMap::new());
        Namespace {
            nodes,
            children,
            by_remote: HashMap::new(),
            next: ROOT + 1,
            local_seq: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }
    pub fn is_empty(&self) -> bool {
        self.nodes.len() <= 1
    }

    pub fn get(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(&id)
    }
    pub fn get_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        self.nodes.get_mut(&id)
    }
    pub fn node_ids(&self) -> Vec<NodeId> {
        self.nodes.keys().copied().collect()
    }

    pub fn by_remote(&self, is_dir: bool, rid: &str) -> Option<NodeId> {
        self.by_remote.get(&rkey(is_dir, rid)).copied()
    }

    /// A fresh key for a file that has no server id yet.
    pub fn new_local_cache_key(&mut self) -> String {
        self.local_seq += 1;
        format!(
            "local-{}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_millis(),
            self.local_seq
        )
    }

    /// Child by name, case-insensitive. A directory wins over a file of the
    /// same name (the server allows both; Windows can't show both).
    pub fn child(&self, parent: NodeId, name: &str) -> Option<NodeId> {
        let ids = self.children.get(&parent)?.get(&fold(name))?;
        ids.iter()
            .copied()
            .find(|i| self.nodes[i].is_dir)
            .or_else(|| ids.first().copied())
    }

    pub fn lookup(&self, path: &str) -> Lookup {
        let parts = split_path(path);
        let mut cur = ROOT;
        for (i, p) in parts.iter().enumerate() {
            match self.child(cur, p) {
                Some(c) => {
                    if i + 1 < parts.len() && !self.nodes[&c].is_dir {
                        return Lookup::PathNotFound;
                    }
                    cur = c;
                }
                None => {
                    return if i + 1 == parts.len() {
                        Lookup::NotFound
                    } else {
                        Lookup::PathNotFound
                    }
                }
            }
        }
        Lookup::Found(cur)
    }

    /// Resolve the parent directory of `path` and return it with the final
    /// component. None if the parent doesn't exist or is a file, or `path`
    /// is the root.
    pub fn parent_of_path<'a>(&self, path: &'a str) -> Option<(NodeId, &'a str)> {
        let parts = split_path(path);
        let (last, dirs) = parts.split_last()?;
        let mut cur = ROOT;
        for d in dirs {
            let c = self.child(cur, d)?;
            if !self.nodes[&c].is_dir {
                return None;
            }
            cur = c;
        }
        Some((cur, last))
    }

    pub fn path_of(&self, id: NodeId) -> String {
        let mut parts = Vec::new();
        let mut cur = id;
        let mut guard = 0;
        while cur != ROOT && guard < 4096 {
            let Some(n) = self.nodes.get(&cur) else { break };
            parts.push(n.name.as_str());
            cur = n.parent;
            guard += 1;
        }
        if parts.is_empty() {
            return "\\".to_string();
        }
        parts.reverse();
        let mut s = String::new();
        for p in parts {
            s.push('\\');
            s.push_str(p);
        }
        s
    }

    /// Visible children: one per folded name (dir preferred), sorted by name.
    pub fn list(&self, dir: NodeId) -> Vec<NodeId> {
        let Some(map) = self.children.get(&dir) else {
            return Vec::new();
        };
        let mut out: Vec<NodeId> = map
            .values()
            .filter_map(|ids| {
                ids.iter()
                    .copied()
                    .find(|i| self.nodes[i].is_dir)
                    .or_else(|| ids.first().copied())
            })
            .collect();
        out.sort_by(|a, b| self.nodes[a].folded.cmp(&self.nodes[b].folded));
        out
    }

    /// Every child, including ones hidden behind a same-named sibling.
    pub fn all_children(&self, dir: NodeId) -> Vec<NodeId> {
        self.children
            .get(&dir)
            .map(|m| m.values().flatten().copied().collect())
            .unwrap_or_default()
    }

    pub fn is_empty_dir(&self, dir: NodeId) -> bool {
        self.children.get(&dir).map(|m| m.is_empty()).unwrap_or(true)
    }

    pub fn is_ancestor(&self, ancestor: NodeId, mut id: NodeId) -> bool {
        let mut guard = 0;
        while guard < 4096 {
            if id == ancestor {
                return true;
            }
            if id == ROOT {
                return false;
            }
            id = match self.nodes.get(&id) {
                Some(n) => n.parent,
                None => return false,
            };
            guard += 1;
        }
        false
    }

    fn link(&mut self, id: NodeId) {
        let (parent, folded) = {
            let n = &self.nodes[&id];
            (n.parent, n.folded.clone())
        };
        self.children
            .entry(parent)
            .or_default()
            .entry(folded)
            .or_default()
            .push(id);
    }

    fn unlink(&mut self, id: NodeId) {
        let (parent, folded) = {
            let n = &self.nodes[&id];
            (n.parent, n.folded.clone())
        };
        if let Some(m) = self.children.get_mut(&parent) {
            if let Some(v) = m.get_mut(&folded) {
                v.retain(|x| *x != id);
                if v.is_empty() {
                    m.remove(&folded);
                }
            }
        }
    }

    /// Insert a node (id is assigned). Returns the new id.
    #[allow(clippy::too_many_arguments)]
    pub fn insert(
        &mut self,
        parent: NodeId,
        name: &str,
        is_dir: bool,
        remote_id: Option<String>,
        size: u64,
        created: u64,
        modified: u64,
        remote_tag: String,
        cache_key: String,
    ) -> NodeId {
        let id = self.next;
        self.next += 1;
        let remote_parent = self.nodes.get(&parent).and_then(|p| p.remote_id.clone());
        let node = Node {
            id,
            is_dir,
            name: name.to_string(),
            folded: fold(name),
            parent,
            remote_id: remote_id.clone(),
            remote_name: name.to_string(),
            remote_parent,
            shadow: false,
            size,
            created,
            modified,
            remote_tag,
            dirty: false,
            gen: 0,
            cache_key,
            open_count: 0,
        };
        self.nodes.insert(id, node);
        if is_dir {
            self.children.insert(id, HashMap::new());
        }
        self.link(id);
        if let Some(r) = remote_id {
            self.by_remote.insert(rkey(is_dir, &r), id);
        }
        id
    }

    pub fn set_remote_id(&mut self, id: NodeId, rid: Option<String>) {
        let Some(n) = self.nodes.get_mut(&id) else { return };
        let is_dir = n.is_dir;
        let old = std::mem::replace(&mut n.remote_id, rid.clone());
        if let Some(o) = old {
            if self.by_remote.get(&rkey(is_dir, &o)) == Some(&id) {
                self.by_remote.remove(&rkey(is_dir, &o));
            }
        }
        if let Some(r) = rid {
            self.by_remote.insert(rkey(is_dir, &r), id);
        }
    }

    /// Move/rename locally. Does not touch the server fields.
    pub fn move_node(&mut self, id: NodeId, new_parent: NodeId, new_name: &str) {
        if id == ROOT {
            return;
        }
        self.unlink(id);
        {
            let n = self.nodes.get_mut(&id).unwrap();
            n.parent = new_parent;
            n.name = new_name.to_string();
            n.folded = fold(new_name);
        }
        self.link(id);
    }

    /// Remove a node and its subtree; returns the removed nodes.
    pub fn remove(&mut self, id: NodeId) -> Vec<Node> {
        if id == ROOT || !self.nodes.contains_key(&id) {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut stack = vec![id];
        let mut order = Vec::new();
        while let Some(x) = stack.pop() {
            order.push(x);
            stack.extend(self.all_children(x));
        }
        self.unlink(id);
        for x in order {
            if let Some(n) = self.nodes.remove(&x) {
                self.children.remove(&x);
                if let Some(r) = &n.remote_id {
                    if self.by_remote.get(&rkey(n.is_dir, r)) == Some(&x) {
                        self.by_remote.remove(&rkey(n.is_dir, r));
                    }
                }
                out.push(n);
            }
        }
        out
    }

    fn subtree_has_local_work(&self, id: NodeId) -> bool {
        let mut stack = vec![id];
        while let Some(x) = stack.pop() {
            if let Some(n) = self.nodes.get(&x) {
                if n.is_file() && (n.dirty || n.remote_id.is_none()) && !crate::util::is_temp_name(&n.name) {
                    return true;
                }
            }
            stack.extend(self.all_children(x));
        }
        false
    }

    /// Merge a freshly fetched server tree into the namespace.
    pub fn apply_remote(&mut self, tree: &Tree) -> ApplyResult {
        let mut res = ApplyResult::default();
        let rfolders: HashMap<&str, &RemoteFolder> =
            tree.folders.iter().map(|f| (f.folder_id.as_str(), f)).collect();
        let rfiles: HashSet<&str> = tree.files.iter().map(|f| f.file_id.as_str()).collect();

        // 1. Things the server no longer has.
        let mut gone: Vec<NodeId> = self
            .nodes
            .values()
            .filter(|n| n.id != ROOT)
            .filter(|n| match &n.remote_id {
                Some(r) if n.is_dir => !rfolders.contains_key(r.as_str()),
                Some(r) => !rfiles.contains(r.as_str()),
                None => false,
            })
            .map(|n| n.id)
            .collect();
        // Deepest first so a folder's removal doesn't pre-empt its children's.
        gone.sort_by_key(|id| std::cmp::Reverse(self.path_of(*id).len()));
        for id in gone {
            let Some(n) = self.nodes.get(&id) else { continue };
            let keep = if n.is_dir {
                self.subtree_has_local_work(id)
            } else {
                n.dirty
            };
            if keep {
                // Local edits/new files inside: keep them and re-create on
                // the server rather than lose data.
                self.set_remote_id(id, None);
                if let Some(n) = self.nodes.get_mut(&id) {
                    n.remote_tag.clear();
                    n.shadow = false;
                    n.dirty = n.is_file() || n.dirty;
                }
                continue;
            }
            let path = self.path_of(id);
            let dir = n.is_dir;
            let rid = n.remote_id.clone().unwrap_or_default();
            let removed = self.remove(id);
            for r in removed {
                if r.is_file() && !r.cache_key.is_empty() {
                    res.removed.push(r.cache_key);
                }
            }
            res.changes.push(Change::Removed { path, dir, rid });
        }

        // 2. Folders, parents first.
        let mut placed: HashMap<String, NodeId> = HashMap::new();
        for f in &tree.folders {
            self.place_folder(&f.folder_id, &rfolders, &mut placed, &mut res, 0);
        }

        // 3. Files.
        for f in &tree.files {
            self.place_file(f, &mut res);
        }
        res
    }

    fn place_folder(
        &mut self,
        fid: &str,
        rfolders: &HashMap<&str, &RemoteFolder>,
        placed: &mut HashMap<String, NodeId>,
        res: &mut ApplyResult,
        depth: usize,
    ) -> NodeId {
        if let Some(id) = placed.get(fid) {
            return *id;
        }
        let Some(f) = rfolders.get(fid) else { return ROOT };
        if depth > 512 {
            return ROOT;
        }
        let parent = match &f.parent_id {
            Some(p) if rfolders.contains_key(p.as_str()) => {
                self.place_folder(p, rfolders, placed, res, depth + 1)
            }
            _ => ROOT,
        };
        let shown = display_name(&f.name);
        let created = f
            .created_at
            .as_deref()
            .and_then(iso_to_filetime)
            .unwrap_or_else(now_filetime);
        let id = if let Some(id) = self.by_remote(true, fid) {
            let (shadow, cur_parent, cur_name) = {
                let n = &self.nodes[&id];
                (n.shadow, n.parent, n.name.clone())
            };
            if !shadow && (cur_parent != parent || cur_name != shown) && !self.is_ancestor(id, parent) {
                let from = self.path_of(id);
                self.move_node(id, parent, &shown);
                let to = self.path_of(id);
                res.changes.push(Change::Renamed { from, to, dir: true, rid: fid.to_string() });
            }
            id
        } else {
            let id = self.insert(
                parent,
                &shown,
                true,
                Some(fid.to_string()),
                0,
                created,
                created,
                String::new(),
                String::new(),
            );
            res.changes.push(Change::Added {
                path: self.path_of(id),
                dir: true,
                rid: fid.to_string(),
            });
            id
        };
        if let Some(n) = self.nodes.get_mut(&id) {
            n.remote_name = f.name.clone();
            n.remote_parent = f.parent_id.clone();
        }
        placed.insert(fid.to_string(), id);
        id
    }

    fn place_file(&mut self, f: &RemoteFile, res: &mut ApplyResult) {
        let parent = match &f.folder_id {
            Some(p) => match self.by_remote(true, p) {
                Some(id) => id,
                None => return, // orphan on the server; skip
            },
            None => ROOT,
        };
        let modified = f
            .updated_at
            .as_deref()
            .and_then(iso_to_filetime)
            .unwrap_or_else(now_filetime);
        let tag = remote_tag(f.size_bytes, f.updated_at.as_deref());
        let shown = display_name(&f.name);
        if let Some(id) = self.by_remote(false, &f.file_id) {
            let (shadow, cur_parent, cur_name) = {
                let n = &self.nodes[&id];
                (n.shadow, n.parent, n.name.clone())
            };
            if !shadow && (cur_parent != parent || cur_name != shown) {
                let from = self.path_of(id);
                self.move_node(id, parent, &shown);
                let to = self.path_of(id);
                res.changes.push(Change::Renamed { from, to, dir: false, rid: f.file_id.clone() });
            }
            let n = self.nodes.get_mut(&id).unwrap();
            n.remote_name = f.name.clone();
            n.remote_parent = f.folder_id.clone();
            if n.remote_tag != tag {
                if n.dirty {
                    tracing::warn!(
                        "{} changed on the server while it has unsent local edits; local copy wins",
                        f.name
                    );
                } else {
                    n.size = f.size_bytes;
                    n.modified = modified;
                    n.remote_tag = tag;
                    res.invalidated.push(n.cache_key.clone());
                    let path = self.path_of(id);
                    res.changes.push(Change::Modified { path, rid: f.file_id.clone() });
                }
            }
        } else {
            let id = self.insert(
                parent,
                &shown,
                false,
                Some(f.file_id.clone()),
                f.size_bytes,
                modified,
                modified,
                tag,
                f.file_id.clone(),
            );
            let n = self.nodes.get_mut(&id).unwrap();
            n.remote_parent = f.folder_id.clone();
            n.remote_name = f.name.clone();
            res.changes.push(Change::Added {
                path: self.path_of(id),
                dir: false,
                rid: f.file_id.clone(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(id: &str, parent: Option<&str>, name: &str) -> RemoteFolder {
        RemoteFolder {
            folder_id: id.into(),
            parent_id: parent.map(|s| s.into()),
            name: name.into(),
            created_at: Some("2026-01-01T00:00:00Z".into()),
        }
    }
    fn file(id: &str, parent: Option<&str>, name: &str, size: u64, upd: &str) -> RemoteFile {
        RemoteFile {
            file_id: id.into(),
            folder_id: parent.map(|s| s.into()),
            name: name.into(),
            size_bytes: size,
            content_type: None,
            updated_at: Some(upd.into()),
        }
    }
    fn tree(folders: Vec<RemoteFolder>, files: Vec<RemoteFile>) -> Tree {
        Tree {
            storage_id: "s".into(),
            etag: "e".into(),
            folders,
            files,
        }
    }

    #[test]
    fn maps_flat_tree_to_paths() {
        let mut ns = Namespace::new();
        // Child listed before its parent on purpose.
        let t = tree(
            vec![folder("f2", Some("f1"), "Sub"), folder("f1", None, "Docs")],
            vec![
                file("a", None, "root.txt", 3, "2026-01-01T00:00:00Z"),
                file("b", Some("f2"), "Deep.TXT", 5, "2026-01-01T00:00:00Z"),
            ],
        );
        let r = ns.apply_remote(&t);
        assert_eq!(r.changes.len(), 4);
        let Lookup::Found(id) = ns.lookup("\\docs\\SUB\\deep.txt") else {
            panic!("case-insensitive lookup failed")
        };
        assert_eq!(ns.path_of(id), "\\Docs\\Sub\\Deep.TXT");
        assert_eq!(ns.get(id).unwrap().size, 5);
        assert_eq!(ns.lookup("\\Docs\\nope"), Lookup::NotFound);
        assert_eq!(ns.lookup("\\nope\\x"), Lookup::PathNotFound);
        assert_eq!(ns.lookup("\\root.txt\\x"), Lookup::PathNotFound);
        assert_eq!(ns.list(ROOT).len(), 2);
    }

    #[test]
    fn diff_rename_modify_remove() {
        let mut ns = Namespace::new();
        ns.apply_remote(&tree(
            vec![folder("f1", None, "Docs")],
            vec![
                file("a", None, "a.txt", 3, "t1"),
                file("b", Some("f1"), "b.txt", 3, "t1"),
            ],
        ));
        let r = ns.apply_remote(&tree(
            vec![folder("f1", None, "Papers")],
            vec![file("a", Some("f1"), "a2.txt", 4, "t2")],
        ));
        assert!(r.changes.contains(&Change::Removed {
            path: "\\Docs\\b.txt".into(),
            dir: false,
            rid: "b".into()
        }));
        assert!(r.changes.contains(&Change::Renamed {
            from: "\\Docs".into(),
            to: "\\Papers".into(),
            dir: true,
            rid: "f1".into()
        }));
        assert!(r.changes.contains(&Change::Renamed {
            from: "\\a.txt".into(),
            to: "\\Papers\\a2.txt".into(),
            dir: false,
            rid: "a".into()
        }));
        assert!(r.changes.contains(&Change::Modified {
            path: "\\Papers\\a2.txt".into(),
            rid: "a".into()
        }));
        assert_eq!(r.invalidated, vec!["a".to_string()]);
        assert_eq!(r.removed, vec!["b".to_string()]);
        // No-op apply produces nothing.
        let r = ns.apply_remote(&tree(
            vec![folder("f1", None, "Papers")],
            vec![file("a", Some("f1"), "a2.txt", 4, "t2")],
        ));
        assert!(r.changes.is_empty());
    }

    #[test]
    fn keeps_local_work_when_server_deletes() {
        let mut ns = Namespace::new();
        ns.apply_remote(&tree(
            vec![folder("f1", None, "Docs")],
            vec![file("a", Some("f1"), "a.txt", 3, "t1")],
        ));
        let Lookup::Found(a) = ns.lookup("\\Docs\\a.txt") else { panic!() };
        ns.get_mut(a).unwrap().dirty = true;
        let key = ns.new_local_cache_key();
        let Lookup::Found(d) = ns.lookup("\\Docs") else { panic!() };
        ns.insert(d, "new.txt", false, None, 0, 0, 0, String::new(), key);
        let r = ns.apply_remote(&tree(vec![], vec![]));
        assert!(r.changes.is_empty());
        // Folder and dirty file survive as local-only, to be re-created.
        assert!(ns.get(d).unwrap().remote_id.is_none());
        assert!(ns.get(a).unwrap().remote_id.is_none());
        assert!(ns.get(a).unwrap().dirty);
    }

    #[test]
    fn shadow_is_not_renamed_back() {
        let mut ns = Namespace::new();
        ns.apply_remote(&tree(vec![], vec![file("a", None, "Doc.docx", 3, "t1")]));
        let Lookup::Found(a) = ns.lookup("\\Doc.docx") else { panic!() };
        ns.move_node(a, ROOT, "~WRL0001.tmp");
        ns.get_mut(a).unwrap().shadow = true;
        let r = ns.apply_remote(&tree(vec![], vec![file("a", None, "Doc.docx", 3, "t1")]));
        assert!(r.changes.is_empty());
        assert_eq!(ns.path_of(a), "\\~WRL0001.tmp");
    }

    #[test]
    fn windows_invalid_names_are_mapped_and_stable() {
        let mut ns = Namespace::new();
        let t = tree(vec![], vec![file("a", None, "what?: \"x\".txt", 3, "t1")]);
        ns.apply_remote(&t);
        let Lookup::Found(id) = ns.lookup("\\what\u{FF1F}\u{FF1A} \u{FF02}x\u{FF02}.txt") else {
            panic!()
        };
        assert_eq!(ns.get(id).unwrap().remote_name, "what?: \"x\".txt");
        // Re-applying must not look like a rename.
        assert!(ns.apply_remote(&t).changes.is_empty());
    }

    #[test]
    fn case_duplicates_are_tolerated() {
        let mut ns = Namespace::new();
        ns.apply_remote(&tree(
            vec![],
            vec![file("a", None, "a.txt", 1, "t"), file("b", None, "A.TXT", 2, "t")],
        ));
        assert_eq!(ns.list(ROOT).len(), 1);
        assert!(matches!(ns.lookup("\\a.txt"), Lookup::Found(_)));
    }

    #[test]
    fn dir_wins_name_clash() {
        let mut ns = Namespace::new();
        ns.apply_remote(&tree(
            vec![folder("f1", None, "X")],
            vec![file("a", None, "x", 3, "t1")],
        ));
        let Lookup::Found(id) = ns.lookup("\\x") else { panic!() };
        assert!(ns.get(id).unwrap().is_dir);
        assert_eq!(ns.list(ROOT).len(), 1);
    }

    #[test]
    fn swap_parentage() {
        let mut ns = Namespace::new();
        ns.apply_remote(&tree(
            vec![folder("A", None, "A"), folder("B", Some("A"), "B")],
            vec![],
        ));
        ns.apply_remote(&tree(
            vec![folder("B", None, "B"), folder("A", Some("B"), "A")],
            vec![],
        ));
        let Lookup::Found(_) = ns.lookup("\\B\\A") else { panic!("{:?}", ns.lookup("\\B\\A")) };
    }
}
