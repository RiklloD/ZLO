//! Complete JSONL lines from a byte offset, in bounded memory: a line longer
//! than [`MAX_LINE`] is skipped whole (counted, never buffered past the cap),
//! and a trailing line without its newline (a live writer mid-line) is left
//! for the next read, so [`Lines::end`] is always a line boundary. Reading
//! stops at a byte limit (the size the file was listed at), so a file that
//! keeps growing while it is read never keeps the reader going.

use std::io::SeekFrom;
use std::path::Path;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, BufReader, Take};

/// Longest line kept (a pasted image inlined as base64 can exceed it).
pub(crate) const MAX_LINE: usize = 8 * 1024 * 1024;

pub(crate) struct Lines {
    reader: BufReader<Take<tokio::fs::File>>,
    /// Bytes consumed from the start of the file.
    offset: u64,
    /// Offset after the last complete line.
    end: u64,
    buf: Vec<u8>,
    oversized: bool,
    max: usize,
    /// Lines skipped for length.
    pub(crate) skipped: u32,
}

impl Lines {
    pub(crate) async fn open(path: &Path, from: u64) -> std::io::Result<Self> {
        Self::open_capped(path, from, MAX_LINE).await
    }

    pub(crate) async fn open_capped(path: &Path, from: u64, max: usize) -> std::io::Result<Self> {
        let file = tokio::fs::File::open(path).await?;
        Self::from_file(file, from, u64::MAX, max).await
    }

    /// Lines of an opened file from `from`, reading at most `limit` bytes.
    pub(crate) async fn from_file(
        mut file: tokio::fs::File,
        from: u64,
        limit: u64,
        max: usize,
    ) -> std::io::Result<Self> {
        file.seek(SeekFrom::Start(from)).await?;
        Ok(Self {
            reader: BufReader::with_capacity(64 * 1024, file.take(limit)),
            offset: from,
            end: from,
            buf: Vec::new(),
            oversized: false,
            max,
            skipped: 0,
        })
    }

    /// The next complete line, without its newline; `None` at the end.
    pub(crate) async fn next_line(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        loop {
            let chunk = self.reader.fill_buf().await?;
            if chunk.is_empty() {
                return Ok(None);
            }
            match chunk.iter().position(|b| *b == b'\n') {
                Some(i) => {
                    if !self.oversized {
                        if self.buf.len() + i > self.max {
                            self.oversized = true;
                        } else {
                            self.buf.extend_from_slice(&chunk[..i]);
                        }
                    }
                    self.reader.consume(i + 1);
                    self.offset += (i + 1) as u64;
                    self.end = self.offset;
                    if std::mem::take(&mut self.oversized) {
                        self.buf.clear();
                        self.skipped += 1;
                        continue;
                    }
                    return Ok(Some(std::mem::take(&mut self.buf)));
                }
                None => {
                    let n = chunk.len();
                    if !self.oversized {
                        if self.buf.len() + n > self.max {
                            self.oversized = true;
                            self.buf = Vec::new();
                        } else {
                            self.buf.extend_from_slice(chunk);
                        }
                    }
                    self.reader.consume(n);
                    self.offset += n as u64;
                }
            }
        }
    }

    /// Offset after the last complete line read.
    pub(crate) fn end(&self) -> u64 {
        self.end
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn long_lines_are_skipped_and_a_torn_tail_is_left() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.jsonl");
        let long = "x".repeat(100);
        tokio::fs::write(&path, format!("a\n{long}\nb\ntorn"))
            .await
            .unwrap();
        let mut lines = Lines::open_capped(&path, 0, 10).await.unwrap();
        assert_eq!(lines.next_line().await.unwrap().unwrap(), b"a");
        assert_eq!(lines.next_line().await.unwrap().unwrap(), b"b");
        assert_eq!(lines.next_line().await.unwrap(), None);
        assert_eq!(lines.skipped, 1);
        assert_eq!(lines.end(), (2 + 101 + 2) as u64);
        // Resuming from the boundary reads the line once it is finished.
        tokio::fs::write(&path, format!("a\n{long}\nb\ntorn\n"))
            .await
            .unwrap();
        let mut rest = Lines::open_capped(&path, lines.end(), 10).await.unwrap();
        assert_eq!(rest.next_line().await.unwrap().unwrap(), b"torn");
        // A byte limit ends the read at the listed size, mid-line or not.
        let file = tokio::fs::File::open(&path).await.unwrap();
        let mut bounded = Lines::from_file(file, 0, 4, 10).await.unwrap();
        assert_eq!(bounded.next_line().await.unwrap().unwrap(), b"a");
        assert_eq!(bounded.next_line().await.unwrap(), None);
        assert_eq!(bounded.end(), 2);
    }
}
