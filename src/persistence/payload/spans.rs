// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;
#[derive(Debug, serde::Serialize)]
pub struct Span {
    pub index: usize,
    pub local: u64,
    pub position: usize,
    pub length: usize,
    pub padding: bool,
    pub skipped: bool,
}
impl MultiFileInfo {
    pub fn spans(&self, offset: u64, length: usize) -> Result<Vec<Span>, StorageError> {
        validate_io_span(self, offset, length as u64, "payload")?;
        let end = offset + length as u64;
        let mut expected = 0u64;
        let mut result = Vec::new();
        for (index, file) in self.files.iter().enumerate() {
            if file.global_start_offset != expected {
                return Err(capability::invalid("noncontiguous payload layout"));
            }
            let file_end = expected
                .checked_add(file.length)
                .ok_or_else(|| capability::invalid("payload layout overflow"))?;
            let start = offset.max(expected);
            let stop = end.min(file_end);
            if start < stop {
                result.push(Span {
                    index,
                    local: start - expected,
                    position: (start - offset) as usize,
                    length: (stop - start) as usize,
                    padding: file.is_padding,
                    skipped: file.is_skipped,
                });
            }
            expected = file_end;
        }
        if expected != self.total_size {
            return Err(capability::invalid("payload total does not match layout"));
        }
        Ok(result)
    }
}
