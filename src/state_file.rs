//! Small helpers for reading/writing a single file inside the preopened
//! `/state` directory (used to persist `token.json`).

use crate::bindings::wasi::filesystem::types::{Descriptor, DescriptorFlags, OpenFlags, PathFlags};

pub fn read_file(dir: &Descriptor, name: &str) -> Result<Option<Vec<u8>>, String> {
    let file = match dir.open_at(
        PathFlags::empty(),
        name,
        OpenFlags::empty(),
        DescriptorFlags::READ,
    ) {
        Ok(f) => f,
        Err(_) => return Ok(None), // treat "doesn't exist" (and any open error) as "no state yet"
    };

    let mut contents = Vec::new();
    let mut offset: u64 = 0;
    loop {
        let (chunk, eof) = file
            .read(64 * 1024, offset)
            .map_err(|e| format!("failed reading {name}: {e:?}"))?;
        offset += chunk.len() as u64;
        contents.extend_from_slice(&chunk);
        if eof || chunk.is_empty() {
            break;
        }
    }
    Ok(Some(contents))
}

/// Overwrites `name` in place. WASI Preview 2 has no chmod, so a newly
/// created file follows the host umask; `tools/device-code-login.sh`
/// creates `token.json` as `0600` first so a later refresh keeps that mode.
pub fn write_file(dir: &Descriptor, name: &str, contents: &[u8]) -> Result<(), String> {
    let file = dir
        .open_at(
            PathFlags::empty(),
            name,
            OpenFlags::CREATE | OpenFlags::TRUNCATE,
            DescriptorFlags::WRITE,
        )
        .map_err(|e| format!("failed opening {name} for write: {e:?}"))?;

    let mut offset: u64 = 0;
    while (offset as usize) < contents.len() {
        let written = file
            .write(&contents[offset as usize..], offset)
            .map_err(|e| format!("failed writing {name}: {e:?}"))?;
        if written == 0 {
            return Err(format!("short write to {name}"));
        }
        offset += written;
    }
    Ok(())
}

/// Reads up to `len` bytes starting at `offset`. Returns `None` if the file
/// cannot be opened (typically because it does not exist yet).
pub fn read_at(
    dir: &Descriptor,
    name: &str,
    offset: u64,
    len: u64,
) -> Result<Option<Vec<u8>>, String> {
    let file = match dir.open_at(
        PathFlags::empty(),
        name,
        OpenFlags::empty(),
        DescriptorFlags::READ,
    ) {
        Ok(f) => f,
        Err(_) => return Ok(None),
    };

    let mut contents = Vec::new();
    let mut pos = offset;
    let end = offset.saturating_add(len);
    while pos < end {
        let want = (end - pos).min(64 * 1024);
        let (chunk, eof) = file
            .read(want, pos)
            .map_err(|e| format!("failed reading {name} at {pos}: {e:?}"))?;
        pos += chunk.len() as u64;
        contents.extend_from_slice(&chunk);
        if eof || chunk.is_empty() {
            break;
        }
    }
    Ok(Some(contents))
}

/// Writes `name` via `{name}.tmp` + `rename-at` so readers never see a
/// truncated snapshot.
pub fn write_atomic(dir: &Descriptor, name: &str, contents: &[u8]) -> Result<(), String> {
    let tmp = format!("{name}.tmp");
    write_file(dir, &tmp, contents)?;
    match dir.rename_at(&tmp, dir, name) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = dir.unlink_file_at(&tmp);
            Err(format!("failed renaming {tmp} -> {name}: {e:?}"))
        }
    }
}

pub fn rename(dir: &Descriptor, from: &str, to: &str) -> Result<(), String> {
    dir.rename_at(from, dir, to)
        .map_err(|e| format!("failed renaming {from} -> {to}: {e:?}"))
}
