//! Segmented on-disk snapshot of [`crate::index::Index`].
//!
//! Layout (little-endian):
//!   header 32 bytes: magic "ODIX" | u32 version | u64 dir_table_off
//!                    | u32 dir_count | u64 meta_off | u32 meta_len
//!   child blocks (per directory, names sorted)
//!   directory table (paths sorted)
//!   meta JSON

use crate::bindings::wasi::filesystem::types::Descriptor;
use crate::index::{Index, ItemMeta, Node, normalize_path, parent_path};
use crate::state_file;
use serde::{Deserialize, Serialize};

pub const INDEX_FILE: &str = "index.bin";
pub const REBUILD_FILE: &str = "index.rebuild.bin";

const MAGIC: &[u8; 4] = b"ODIX";
const VERSION: u32 = 1;
const HEADER_LEN: usize = 32;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotMeta {
    pub delta_token: Option<String>,
    pub pending_next_link: Option<String>,
    pub crawl_complete: bool,
    pub generation: u64,
    pub built_at: u64,
    pub root_id: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ChildInfo {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: u64,
    pub etag: String,
    pub id: String,
}

#[derive(Clone, Debug)]
pub struct DirRow {
    pub path: String,
    pub block_off: u64,
    pub block_len: u32,
    pub id: String,
    pub size: u64,
    pub mtime: u64,
    pub etag: String,
}

#[derive(Clone, Debug)]
pub enum Lookup {
    File(ChildInfo),
    Dir {
        info: ChildInfo,
        children: Vec<ChildInfo>,
    },
}

pub fn encode(index: &Index) -> Result<Vec<u8>, String> {
    let mut reachable: Vec<(&str, String)> = Vec::new();
    for id in index.nodes.keys() {
        if let Some(path) = index.path_of(id) {
            reachable.push((id.as_str(), path));
        }
    }
    let mut dirs: Vec<(&str, String, &Node)> = reachable
        .iter()
        .filter_map(|(id, path)| {
            let node = index.nodes.get(*id)?;
            if node.meta.is_dir || index.root.as_deref() == Some(*id) {
                Some((*id, path.clone(), node))
            } else {
                None
            }
        })
        .collect();
    dirs.sort_by(|a, b| a.1.cmp(&b.1));

    let mut body = vec![0u8; HEADER_LEN];
    let mut rows: Vec<DirRow> = Vec::with_capacity(dirs.len());

    for (id, path, node) in &dirs {
        let block_off = body.len() as u64;
        let kids = index.kids.get(*id);
        if let Some(set) = kids {
            for (name, cid) in set {
                let Some(child) = index.nodes.get(cid) else {
                    continue;
                };
                write_child(
                    &mut body,
                    &ChildInfo {
                        name: name.clone(),
                        is_dir: child.meta.is_dir,
                        size: child.meta.size,
                        mtime: child.meta.mtime,
                        etag: child.meta.etag.clone(),
                        id: cid.clone(),
                    },
                );
            }
        }
        let block_len = (body.len() as u64 - block_off) as u32;
        rows.push(DirRow {
            path: path.clone(),
            block_off,
            block_len,
            id: (*id).to_string(),
            size: node.meta.size,
            mtime: node.meta.mtime,
            etag: node.meta.etag.clone(),
        });
    }

    let dir_table_off = body.len() as u64;
    for row in &rows {
        write_dir_row(&mut body, row);
    }

    let meta = SnapshotMeta {
        delta_token: index.delta_token.clone(),
        pending_next_link: index.pending_next_link.clone(),
        crawl_complete: index.crawl_complete,
        generation: index.generation,
        built_at: 0,
        root_id: index.root.clone(),
    };
    let meta_bytes = serde_json::to_vec(&meta).map_err(|e| e.to_string())?;
    let meta_off = body.len() as u64;
    body.extend_from_slice(&meta_bytes);

    write_header(
        &mut body,
        dir_table_off,
        rows.len() as u32,
        meta_off,
        meta_bytes.len() as u32,
    );
    Ok(body)
}

pub fn decode(bytes: &[u8]) -> Result<Index, String> {
    let (dir_table_off, dir_count, meta_off, meta_len) = parse_header(bytes)?;
    let meta = parse_meta(bytes, meta_off, meta_len)?;
    let table_end = meta_off as usize;
    let table = &bytes[dir_table_off as usize..table_end];
    let rows = parse_dir_table(table, dir_count)?;

    let mut index = Index {
        delta_token: meta.delta_token,
        pending_next_link: meta.pending_next_link,
        crawl_complete: meta.crawl_complete,
        generation: meta.generation,
        root: meta.root_id,
        ..Index::default()
    };

    for row in &rows {
        let children = parse_child_block(bytes, row.block_off, row.block_len)?;
        index.nodes.insert(
            row.id.clone(),
            Node {
                parent: None,
                name: crate::index::last_segment(&row.path),
                meta: ItemMeta {
                    is_dir: true,
                    size: row.size,
                    mtime: row.mtime,
                    etag: row.etag.clone(),
                },
            },
        );
        let mut set = BTreeSetLite::new();
        for child in children {
            index.nodes.insert(
                child.id.clone(),
                Node {
                    parent: Some(row.id.clone()),
                    name: child.name.clone(),
                    meta: ItemMeta {
                        is_dir: child.is_dir,
                        size: child.size,
                        mtime: child.mtime,
                        etag: child.etag,
                    },
                },
            );
            set.insert((child.name, child.id));
        }
        index.kids.insert(row.id.clone(), set);
    }

    let path_to_id: HashLite = rows
        .iter()
        .map(|r| (r.path.clone(), r.id.clone()))
        .collect();
    for row in &rows {
        if row.path == "/" {
            index.nodes.entry(row.id.clone()).and_modify(|n| {
                n.parent = None;
                n.name.clear();
            });
            if index.root.is_none() {
                index.root = Some(row.id.clone());
            }
            continue;
        }
        let parent = parent_path(&row.path);
        if let Some(pid) = path_to_id.get(&parent) {
            if let Some(node) = index.nodes.get_mut(&row.id) {
                node.parent = Some(pid.clone());
            }
        }
    }
    Ok(index)
}

/// Lightweight aliases so this module does not import collections at the top
/// for the decode-only maps.
type BTreeSetLite = std::collections::BTreeSet<(String, String)>;
type HashLite = std::collections::HashMap<String, String>;

pub fn load(dir: &Descriptor, name: &str) -> Result<Option<Index>, String> {
    match state_file::read_file(dir, name)? {
        Some(bytes) => Ok(Some(decode(&bytes)?)),
        None => Ok(None),
    }
}

pub fn save(dir: &Descriptor, name: &str, index: &Index) -> Result<(), String> {
    let bytes = encode(index)?;
    state_file::write_atomic(dir, name, &bytes)
}

/// Patch the live snapshot in place. No-op if it is missing or unreadable.
pub fn mutate(dir: &Descriptor, f: impl FnOnce(&mut Index) -> bool) {
    let mut idx = match load(dir, INDEX_FILE) {
        Ok(Some(i)) => i,
        _ => return,
    };
    if f(&mut idx) {
        let _ = save(dir, INDEX_FILE, &idx);
    }
}

#[cfg(test)]
pub fn lookup_bytes(bytes: &[u8], path: &str) -> Result<Option<Lookup>, String> {
    let (dir_table_off, dir_count, meta_off, _meta_len) = parse_header(bytes)?;
    let table_end = meta_off as usize;
    let start = dir_table_off as usize;
    let table = bytes
        .get(start..table_end)
        .ok_or_else(|| "index dir table truncated".to_string())?;
    let rows = parse_dir_table(table, dir_count)?;
    lookup_rows(bytes, &rows, path, true)
}

pub fn lookup_file(dir: &Descriptor, path: &str) -> Result<Option<Lookup>, String> {
    let header = match state_file::read_at(dir, INDEX_FILE, 0, HEADER_LEN as u64)? {
        Some(h) => h,
        None => return Ok(None),
    };
    if header.len() < HEADER_LEN {
        return Err("index header truncated".into());
    }
    let (dir_table_off, dir_count, meta_off, _meta_len) = parse_header(&header)?;
    let table_len = meta_off.saturating_sub(dir_table_off);
    let table_bytes = state_file::read_at(dir, INDEX_FILE, dir_table_off, table_len)?
        .ok_or_else(|| "index dir table missing".to_string())?;
    let rows = parse_dir_table(&table_bytes, dir_count)?;
    lookup_with(rows, path, |row| {
        if row.block_len == 0 {
            return Ok(Vec::new());
        }
        state_file::read_at(dir, INDEX_FILE, row.block_off, row.block_len as u64)?
            .ok_or_else(|| "index child block missing".to_string())
    })
}

#[cfg(test)]
fn lookup_rows(
    bytes: &[u8],
    rows: &[DirRow],
    path: &str,
    _blocks_inline: bool,
) -> Result<Option<Lookup>, String> {
    lookup_with(rows.to_vec(), path, |row| {
        let start = row.block_off as usize;
        let end = start.saturating_add(row.block_len as usize);
        bytes
            .get(start..end)
            .map(|s| s.to_vec())
            .ok_or_else(|| "child block out of range".to_string())
    })
}

fn lookup_with(
    rows: Vec<DirRow>,
    path: &str,
    mut read_block: impl FnMut(&DirRow) -> Result<Vec<u8>, String>,
) -> Result<Option<Lookup>, String> {
    let path = normalize_path(path);
    if let Some(row) = find_dir(&rows, &path) {
        let block = read_block(row)?;
        let children = parse_child_block(&block, 0, block.len() as u32)?;
        return Ok(Some(Lookup::Dir {
            info: ChildInfo {
                name: crate::index::last_segment(&row.path),
                is_dir: true,
                size: row.size,
                mtime: row.mtime,
                etag: row.etag.clone(),
                id: row.id.clone(),
            },
            children,
        }));
    }
    let parent = parent_path(&path);
    let name = crate::index::last_segment(&path);
    let Some(row) = find_dir(&rows, &parent) else {
        return Ok(None);
    };
    let block = read_block(row)?;
    let children = parse_child_block(&block, 0, block.len() as u32)?;
    Ok(children
        .into_iter()
        .find(|c| c.name == name)
        .map(Lookup::File))
}

fn find_dir<'a>(rows: &'a [DirRow], path: &str) -> Option<&'a DirRow> {
    rows.binary_search_by(|r| r.path.as_str().cmp(path))
        .ok()
        .map(|i| &rows[i])
}

fn write_header(buf: &mut [u8], dir_table_off: u64, dir_count: u32, meta_off: u64, meta_len: u32) {
    buf[0..4].copy_from_slice(MAGIC);
    buf[4..8].copy_from_slice(&VERSION.to_le_bytes());
    buf[8..16].copy_from_slice(&dir_table_off.to_le_bytes());
    buf[16..20].copy_from_slice(&dir_count.to_le_bytes());
    buf[20..28].copy_from_slice(&meta_off.to_le_bytes());
    buf[28..32].copy_from_slice(&meta_len.to_le_bytes());
}

fn parse_header(bytes: &[u8]) -> Result<(u64, u32, u64, u32), String> {
    if bytes.len() < HEADER_LEN {
        return Err("index header too short".into());
    }
    if &bytes[0..4] != MAGIC {
        return Err("index magic mismatch".into());
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    if version != VERSION {
        return Err(format!("unsupported index version {version}"));
    }
    let dir_table_off = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    let dir_count = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
    let meta_off = u64::from_le_bytes(bytes[20..28].try_into().unwrap());
    let meta_len = u32::from_le_bytes(bytes[28..32].try_into().unwrap());
    if dir_table_off as usize > bytes.len() || meta_off as usize > bytes.len() {
        // lookup_file passes a 32-byte header only; offsets may exceed that.
        // Bound-check against the slice only when the slice looks like a full file.
        if bytes.len() != HEADER_LEN
            && (meta_off as usize > bytes.len() || dir_table_off as usize > bytes.len())
        {
            return Err("index offsets out of range".into());
        }
    }
    Ok((dir_table_off, dir_count, meta_off, meta_len))
}

fn parse_meta(bytes: &[u8], meta_off: u64, meta_len: u32) -> Result<SnapshotMeta, String> {
    let start = meta_off as usize;
    let end = start
        .checked_add(meta_len as usize)
        .ok_or_else(|| "index meta overflow".to_string())?;
    let slice = bytes
        .get(start..end)
        .ok_or_else(|| "index meta truncated".to_string())?;
    serde_json::from_slice(slice).map_err(|e| format!("index meta: {e}"))
}

fn write_str(buf: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(bytes);
}

fn read_str(data: &[u8], pos: &mut usize) -> Result<String, String> {
    if *pos + 4 > data.len() {
        return Err("truncated string length".into());
    }
    let len = u32::from_le_bytes(data[*pos..*pos + 4].try_into().unwrap()) as usize;
    *pos += 4;
    let end = *pos + len;
    if end > data.len() {
        return Err("truncated string".into());
    }
    let s = std::str::from_utf8(&data[*pos..end]).map_err(|_| "non-utf8 string".to_string())?;
    *pos = end;
    Ok(s.to_string())
}

fn write_child(buf: &mut Vec<u8>, c: &ChildInfo) {
    write_str(buf, &c.name);
    buf.push(if c.is_dir { 1 } else { 0 });
    buf.extend_from_slice(&c.size.to_le_bytes());
    buf.extend_from_slice(&c.mtime.to_le_bytes());
    write_str(buf, &c.etag);
    write_str(buf, &c.id);
}

fn parse_child_block(bytes: &[u8], off: u64, len: u32) -> Result<Vec<ChildInfo>, String> {
    let start = off as usize;
    let end = start + len as usize;
    if end > bytes.len() {
        return Err("child block out of range".into());
    }
    let data = &bytes[start..end];
    let mut pos = 0;
    let mut out = Vec::new();
    while pos < data.len() {
        let name = read_str(data, &mut pos)?;
        if pos >= data.len() {
            return Err("truncated child flags".into());
        }
        let is_dir = data[pos] != 0;
        pos += 1;
        if pos + 16 > data.len() {
            return Err("truncated child numbers".into());
        }
        let size = u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
        pos += 8;
        let mtime = u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
        pos += 8;
        let etag = read_str(data, &mut pos)?;
        let id = read_str(data, &mut pos)?;
        out.push(ChildInfo {
            name,
            is_dir,
            size,
            mtime,
            etag,
            id,
        });
    }
    Ok(out)
}

fn write_dir_row(buf: &mut Vec<u8>, row: &DirRow) {
    write_str(buf, &row.path);
    buf.extend_from_slice(&row.block_off.to_le_bytes());
    buf.extend_from_slice(&row.block_len.to_le_bytes());
    write_str(buf, &row.id);
    buf.extend_from_slice(&row.size.to_le_bytes());
    buf.extend_from_slice(&row.mtime.to_le_bytes());
    write_str(buf, &row.etag);
}

fn parse_dir_table(data: &[u8], count: u32) -> Result<Vec<DirRow>, String> {
    let mut pos = 0;
    let mut rows = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let path = read_str(data, &mut pos)?;
        if pos + 12 > data.len() {
            return Err("truncated dir row".into());
        }
        let block_off = u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
        pos += 8;
        let block_len = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap());
        pos += 4;
        let id = read_str(data, &mut pos)?;
        if pos + 16 > data.len() {
            return Err("truncated dir row meta".into());
        }
        let size = u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
        pos += 8;
        let mtime = u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
        pos += 8;
        let etag = read_str(data, &mut pos)?;
        rows.push(DirRow {
            path,
            block_off,
            block_len,
            id,
            size,
            mtime,
            etag,
        });
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{DeltaItem, Index, ItemMeta};

    fn root() -> DeltaItem {
        DeltaItem {
            id: "r".into(),
            parent_id: None,
            name: String::new(),
            deleted: false,
            is_root: true,
            meta: ItemMeta {
                is_dir: true,
                size: 0,
                mtime: 1,
                etag: "er".into(),
            },
        }
    }

    #[test]
    fn round_trip_preserves_tree_and_token() {
        let mut idx = Index {
            generation: 3,
            crawl_complete: true,
            delta_token: Some(
                "https://graph.microsoft.com/v1.0/me/drive/root/delta?token=abc".into(),
            ),
            ..Index::default()
        };
        idx.apply(root());
        idx.apply(DeltaItem {
            id: "d".into(),
            parent_id: Some("r".into()),
            name: "Docs".into(),
            deleted: false,
            is_root: false,
            meta: ItemMeta {
                is_dir: true,
                size: 0,
                mtime: 2,
                etag: "ed".into(),
            },
        });
        idx.apply(DeltaItem {
            id: "f".into(),
            parent_id: Some("d".into()),
            name: "a.txt".into(),
            deleted: false,
            is_root: false,
            meta: ItemMeta {
                is_dir: false,
                size: 9,
                mtime: 3,
                etag: "ef".into(),
            },
        });
        let bytes = encode(&idx).unwrap();
        let out = decode(&bytes).unwrap();
        assert_eq!(out.root.as_deref(), Some("r"));
        assert_eq!(out.resolve("/Docs/a.txt").as_deref(), Some("f"));
        assert_eq!(out.nodes.get("f").unwrap().meta.size, 9);
        assert_eq!(out.delta_token, idx.delta_token);
        assert!(out.crawl_complete);
        assert_eq!(out.generation, 3);

        let hit = lookup_bytes(&bytes, "/Docs").unwrap().unwrap();
        match hit {
            Lookup::Dir { children, .. } => {
                assert_eq!(children.len(), 1);
                assert_eq!(children[0].name, "a.txt");
            }
            Lookup::File(_) => panic!("expected dir"),
        }
        match lookup_bytes(&bytes, "/Docs/a.txt").unwrap().unwrap() {
            Lookup::File(f) => assert_eq!(f.size, 9),
            Lookup::Dir { .. } => panic!("expected file"),
        }
    }

    #[test]
    fn rejects_bad_magic() {
        let err = decode(b"NOPE").unwrap_err();
        assert!(err.contains("too short") || err.contains("magic"));
    }
}
