//! Raw PTY output journal: `journals/<sid>/NNNN.log`.
//!
//! Record layout (little-endian): `u64 ts_ms | u32 len | len bytes`. A new
//! file is started when the current one would exceed the rotation size; a
//! record larger than the limit gets a file of its own. Reopening a journal
//! continues after the highest existing file.

use std::fs::{self, File};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use crate::fsutil;
use crate::StoreError;

/// Rotation threshold (DESIGN §6: 64 MiB).
pub const JOURNAL_ROTATE_BYTES: u64 = 64 * 1024 * 1024;
const HEADER_LEN: u64 = 12;

/// Append-only raw output log for one session.
pub struct JournalWriter {
    dir: PathBuf,
    index: u32,
    file: BufWriter<File>,
    written: u64,
    max_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalRecord {
    pub at_ms: i64,
    pub bytes: Vec<u8>,
}

impl JournalWriter {
    pub fn open(dir: &Path) -> Result<JournalWriter, StoreError> {
        Self::with_max_bytes(dir, JOURNAL_ROTATE_BYTES)
    }

    /// Like `open` with a custom rotation size (tests, tuning).
    pub fn with_max_bytes(dir: &Path, max_bytes: u64) -> Result<JournalWriter, StoreError> {
        fsutil::ensure_private_dir(dir)?;
        let index = list_files(dir)?.last().map(|(i, _)| *i).unwrap_or(0);
        let path = file_path(dir, index);
        let file = fsutil::open_private_append(&path)?;
        let written = file.metadata()?.len();
        Ok(JournalWriter {
            dir: dir.to_path_buf(),
            index,
            file: BufWriter::new(file),
            written,
            max_bytes: max_bytes.max(HEADER_LEN + 1),
        })
    }

    /// Append a chunk with a millisecond timestamp; rotates at the limit.
    pub fn append(&mut self, at_ms: i64, bytes: &[u8]) -> Result<(), StoreError> {
        let len = u32::try_from(bytes.len())
            .map_err(|_| StoreError::Codec(format!("journal chunk too large: {}", bytes.len())))?;
        let record_len = HEADER_LEN + u64::from(len);
        if self.written > 0 && self.written + record_len > self.max_bytes {
            self.rotate()?;
        }
        self.file.write_all(&(at_ms.max(0) as u64).to_le_bytes())?;
        self.file.write_all(&len.to_le_bytes())?;
        self.file.write_all(bytes)?;
        self.written += record_len;
        Ok(())
    }

    pub fn flush(&mut self) -> Result<(), StoreError> {
        self.file.flush()?;
        Ok(())
    }

    /// Path of the file currently being appended to.
    pub fn current_file(&self) -> PathBuf {
        file_path(&self.dir, self.index)
    }

    fn rotate(&mut self) -> Result<(), StoreError> {
        self.file.flush()?;
        self.index += 1;
        let file = fsutil::open_private_append(&file_path(&self.dir, self.index))?;
        self.file = BufWriter::new(file);
        self.written = 0;
        Ok(())
    }
}

impl Drop for JournalWriter {
    fn drop(&mut self) {
        if let Err(e) = self.file.flush() {
            tracing::warn!(dir = %self.dir.display(), error = %e, "journal flush on drop failed");
        }
    }
}

/// Read every record of a journal directory in order. A truncated record at
/// the end of a file (crash mid-write) ends that file's records.
pub fn read_journal(dir: &Path) -> Result<Vec<JournalRecord>, StoreError> {
    let mut out = Vec::new();
    for (_, path) in list_files(dir)? {
        let mut data = Vec::new();
        File::open(&path)?.read_to_end(&mut data)?;
        let mut pos = 0usize;
        while data.len() - pos >= HEADER_LEN as usize {
            let ts = u64::from_le_bytes(data[pos..pos + 8].try_into().expect("8 bytes"));
            let len =
                u32::from_le_bytes(data[pos + 8..pos + 12].try_into().expect("4 bytes")) as usize;
            let start = pos + HEADER_LEN as usize;
            if data.len() - start < len {
                tracing::warn!(file = %path.display(), "truncated journal record");
                break;
            }
            out.push(JournalRecord {
                at_ms: ts as i64,
                bytes: data[start..start + len].to_vec(),
            });
            pos = start + len;
        }
    }
    Ok(out)
}

fn file_path(dir: &Path, index: u32) -> PathBuf {
    dir.join(format!("{index:04}.log"))
}

/// `(index, path)` of every `NNNN.log`, sorted numerically.
fn list_files(dir: &Path) -> Result<Vec<(u32, PathBuf)>, StoreError> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut files = Vec::new();
    for entry in entries {
        let path = entry?.path();
        let index = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".log"))
            .and_then(|n| n.parse::<u32>().ok());
        if let Some(i) = index {
            files.push((i, path));
        }
    }
    files.sort_by_key(|(i, _)| *i);
    Ok(files)
}
