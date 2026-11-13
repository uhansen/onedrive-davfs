//! Private, unlinked upload spool: bounded memory and automatic close/trap cleanup.

use crate::bindings::wasi::filesystem::types::{Descriptor, DescriptorFlags, OpenFlags, PathFlags};

pub struct Upload {
    file: Descriptor,
    pub size: u64,
}

impl Upload {
    pub fn create(directory: &Descriptor) -> Result<Self, String> {
        let directory = directory
            .open_at(
                PathFlags::empty(),
                ".uploads",
                OpenFlags::DIRECTORY,
                DescriptorFlags::READ,
            )
            .map_err(|error| format!("cannot open private upload directory: {error:?}"))?;
        let name = format!(
            ".upload-{:016x}",
            crate::bindings::wasi::random::random::get_random_u64()
        );
        let file = directory
            .open_at(
                PathFlags::empty(),
                &name,
                OpenFlags::CREATE | OpenFlags::EXCLUSIVE,
                DescriptorFlags::READ | DescriptorFlags::WRITE,
            )
            .map_err(|error| format!("cannot create private upload spool: {error:?}"))?;
        directory
            .unlink_file_at(&name)
            .map_err(|error| format!("cannot unlink upload spool: {error:?}"))?;
        Ok(Self { file, size: 0 })
    }

    pub fn append(&mut self, data: &[u8], limit: u64) -> Result<(), String> {
        check_size(self.size, data.len() as u64, limit)?;
        let mut offset = 0;
        while offset < data.len() {
            let written = self
                .file
                .write(&data[offset..], self.size)
                .map_err(|error| format!("upload spool write failed: {error:?}"))?
                as usize;
            if written == 0 {
                return Err("upload spool short write".into());
            }
            offset += written;
            self.size += written as u64;
        }
        Ok(())
    }

    pub fn read(&self, offset: u64, length: usize) -> Result<Vec<u8>, String> {
        let mut data = Vec::with_capacity(length);
        while data.len() < length {
            let (chunk, _) = self
                .file
                .read(
                    (length - data.len()).min(64 * 1024) as u64,
                    offset + data.len() as u64,
                )
                .map_err(|error| format!("upload spool read failed: {error:?}"))?;
            if chunk.is_empty() {
                return Err("upload spool ended before its declared length".into());
            }
            data.extend_from_slice(&chunk);
        }
        Ok(data)
    }
}

pub fn check_size(current: u64, additional: u64, limit: u64) -> Result<(), String> {
    if current
        .checked_add(additional)
        .is_none_or(|size| size > limit)
    {
        return Err(format!("upload exceeds configured {limit}-byte limit"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_limits_include_unknown_length_and_overflow() {
        assert!(check_size(64, 1, 65).is_ok());
        assert!(check_size(64, 2, 65).is_err());
        assert!(check_size(u64::MAX, 1, u64::MAX).is_err());
    }
}
