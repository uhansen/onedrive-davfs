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
