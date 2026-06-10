//! In-process B1 baseline — the `zstdcat files_in_range.zst | grep TOKEN
//! | wc -l` reference (`docs/benchmarks.md` §3 B1), without shelling out
//! to a system `zstd`/`grep` (so the bench is reproducible on any host;
//! same bundled `zstd` the A1 reference codec uses).
//!
//! **Fairness.** The caller passes only the raw lines from the files that
//! fall in the query's time window — the file-level pruning the baseline
//! pipeline gets "for free" by naming `files_in_range.zst`. Within those
//! files the reference scans **every** line (decode + substring match),
//! whereas the Ourios query additionally skips row groups via column
//! statistics. So the measured ratio reflects *within-window* pruning,
//! not a strawman full-corpus scan against a no-pruning baseline.

/// A B1 reference corpus: the in-window raw log lines, `zstd`-compressed
/// one block per file (mirroring stored `*.zst` segments the baseline
/// `zstdcat`s).
pub struct ReferenceCorpus {
    blocks: Vec<Vec<u8>>,
}

impl ReferenceCorpus {
    /// Assemble from already-compressed per-file zstd blocks (the
    /// streaming path: `build_b1_store` spools lines to disk and
    /// compresses one file at a time, so GiB-class corpora never sit
    /// in memory uncompressed). Each block must hold
    /// newline-terminated lines, like [`ReferenceCorpus::compress`]
    /// produces.
    #[must_use]
    pub fn from_blocks(blocks: Vec<Vec<u8>>) -> Self {
        Self { blocks }
    }

    /// Compress each in-window file's raw lines (`files[i]` is one file's
    /// lines) at `level`. Only in-window files should be passed (see the
    /// module's fairness note).
    ///
    /// # Errors
    ///
    /// Propagates a `zstd` compression I/O error (not expected for an
    /// in-memory buffer, but the encoder's signature is fallible).
    pub fn compress(files: &[Vec<String>], level: i32) -> std::io::Result<Self> {
        use std::io::Write;

        let mut blocks = Vec::with_capacity(files.len());
        for lines in files {
            // Stream each line + `\n` straight into the encoder rather
            // than materialising the whole file as one `String` first:
            // setup stays memory-bounded at GiB-window scale and mirrors
            // how a real `*.zst` segment is produced. Newline-*terminate*
            // every line (as a real log file is), so the decode below and
            // a `zstdcat | wc -l` pipeline agree with no off-by-one.
            let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), level)?;
            for line in lines {
                encoder.write_all(line.as_bytes())?;
                encoder.write_all(b"\n")?;
            }
            blocks.push(encoder.finish()?);
        }
        Ok(Self { blocks })
    }

    /// `zstdcat <blocks> | grep -F token | wc -l`: stream-decode every
    /// block and count the lines containing `token`. Matches at the
    /// **byte** level (like `grep -F`) over a reused line buffer — no
    /// UTF-8 decode and no per-line allocation — so the baseline isn't
    /// artificially slowed (which would bias the B1 ratio in Ourios's
    /// favour). Streaming keeps memory bounded at GiB corpus scale. This
    /// is the timed reference work the B1 bench compares Ourios against.
    ///
    /// # Errors
    ///
    /// Propagates a `zstd` decompression / read error if a block is
    /// corrupt or truncated.
    pub fn count_lines_containing(&self, token: &str) -> std::io::Result<u64> {
        use std::io::BufRead;

        let needle = token.as_bytes();
        let mut matches = 0u64;
        // One line buffer reused across every block (and every line).
        let mut line = Vec::new();
        for block in &self.blocks {
            let decoder = zstd::stream::read::Decoder::new(block.as_slice())?;
            let mut reader = std::io::BufReader::new(decoder);
            loop {
                line.clear();
                if reader.read_until(b'\n', &mut line)? == 0 {
                    break;
                }
                // Match the line *content*, like `grep` — without the
                // trailing `\n` (and a `\r` for CRLF input) the delimiter
                // would otherwise leave in the buffer.
                let mut content = line.as_slice();
                if let [rest @ .., b'\n'] = content {
                    content = rest;
                }
                if let [rest @ .., b'\r'] = content {
                    content = rest;
                }
                if contains_subslice(content, needle) {
                    matches = matches.saturating_add(1);
                }
            }
        }
        Ok(matches)
    }

    /// Total compressed size across the in-window blocks — the `*.zst`
    /// size a `zstdcat` baseline would read. (Held in memory here, so
    /// this measures size, not disk I/O.)
    ///
    /// # Panics
    ///
    /// Panics only if a single block's length exceeds `u64` (`usize >
    /// u64`), which is unreachable on any supported target.
    #[must_use]
    pub fn compressed_bytes(&self) -> u64 {
        self.blocks.iter().fold(0_u64, |acc, b| {
            acc.saturating_add(
                u64::try_from(b.len()).expect("usize fits in u64 on every supported Rust target"),
            )
        })
    }
}

/// Whether `haystack` contains `needle` as a byte substring — the
/// `grep -F` fixed-string match, without UTF-8 decoding. Anchors on the
/// needle's first byte and only compares the remainder on a hit, so it
/// avoids the redundant full-window comparison `windows().any()` would
/// do at every position (keeps the baseline close to `grep -F`'s
/// optimized search rather than artificially slow).
fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    let Some((&first, rest)) = needle.split_first() else {
        return true; // an empty needle matches anywhere
    };
    if needle.len() > haystack.len() {
        return false;
    }
    let last_start = haystack.len() - needle.len();
    // `.get()` (not indexing) keeps this panic-free by construction.
    (0..=last_start).any(|i| {
        haystack.get(i) == Some(&first) && haystack.get(i + 1..i + needle.len()) == Some(rest)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_only_in_window_lines_containing_the_token() {
        // Arrange — two in-window "files": one with 3 ERROR lines + 1
        // INFO, one all INFO. (Out-of-window files are simply not passed.)
        let files = vec![
            vec![
                "ERROR boom a".to_string(),
                "INFO ok".to_string(),
                "ERROR boom b".to_string(),
                "ERROR boom c".to_string(),
            ],
            vec!["INFO ok".to_string(), "INFO also ok".to_string()],
        ];

        // Act
        let reference = ReferenceCorpus::compress(&files, 19).expect("compress");
        let n = reference.count_lines_containing("ERROR").expect("count");

        // Assert — exactly the three ERROR lines, across both files.
        assert_eq!(n, 3);
        assert!(reference.compressed_bytes() > 0, "blocks hold bytes");
    }

    #[test]
    fn empty_corpus_counts_zero() {
        // Arrange / Act
        let reference = ReferenceCorpus::compress(&[], 19).expect("compress");

        // Assert
        assert_eq!(reference.count_lines_containing("ERROR").expect("count"), 0);
        assert_eq!(reference.compressed_bytes(), 0);
    }
}
