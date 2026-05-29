//! `Wal::open` integration tests landing with PR-M4.
//!
//! Covers the cases the §5 acceptance criteria don't yet have
//! a live test for but the `open` implementation needs to
//! satisfy directly: first-run segment creation, existing-
//! segment reopen, all §6.9 tunable validation arms.

use std::io::Read;
use std::path::Path;

use ourios_wal::{
    MAX_BATCH_WINDOW_MS, MAX_HOUSEKEEPING_SECS, MAX_SEGMENT_AGE_SECS, MAX_SEGMENT_SIZE_BYTES,
    MIN_HOUSEKEEPING_SECS, MIN_SEGMENT_AGE_SECS, OpenError, Wal, WalConfig,
};

fn default_config(root: &Path) -> WalConfig {
    WalConfig {
        root: root.to_path_buf(),
        batch_window_ms: 100,
        segment_size_bytes: 128 * 1024 * 1024,
        segment_age_secs: 600,
        housekeeping_secs: 60,
        macos_full_fsync: false,
    }
}

/// A fresh `wal_root` opens cleanly + creates exactly one
/// segment file whose 24 B header carries the §6.2.1 magic +
/// version + flags + `UUIDv7` bytes. Filename and in-file UUID
/// agree (the §6.2.1 "carrying it inside the file means a
/// rename doesn't make the file unreadable" guarantee).
#[test]
fn fresh_root_creates_one_segment_with_a_valid_header() {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let _wal = Wal::open(default_config(tmp.path())).expect("fresh root opens");
    let segments: Vec<_> = std::fs::read_dir(tmp.path())
        .expect("read_dir")
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "wal"))
        .collect();
    assert_eq!(segments.len(), 1, "exactly one segment file created");
    let mut bytes = Vec::new();
    std::fs::File::open(segments[0].path())
        .expect("open")
        .read_to_end(&mut bytes)
        .expect("read");
    // Exact §6.2.1 layout: 24-byte header, then nothing yet
    // (frames land with the `append` slice).
    assert_eq!(bytes.len(), 24, "fresh segment has only the header");
    assert_eq!(&bytes[0..4], b"OWAL", "magic");
    assert_eq!(&bytes[4..6], &[0x01, 0x00], "version = 1");
    assert_eq!(&bytes[6..8], &[0x00, 0x00], "flags = 0");
    // The UUID in the header matches the filename stem — pin
    // the cross-check that lets a renamed file still decode.
    let stem = segments[0]
        .path()
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let uuid_from_name = uuid::Uuid::parse_str(&stem).expect("filename is a parseable UUID");
    assert_eq!(uuid_from_name.as_bytes(), &bytes[8..24]);
    assert_eq!(
        uuid_from_name.get_version_num(),
        7,
        "segment UUID MUST be UUIDv7 — the chronological sort property §6.1 depends on",
    );
}

/// Reopening an existing root doesn't add a fresh segment —
/// the existing newest segment is reused. Pins the
/// "lexicographically-greatest segment = the newest per
/// `UUIDv7`'s chronological sort, open it for further appends"
/// contract `Wal::open`'s rustdoc names.
#[test]
fn existing_root_reuses_newest_segment() {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    drop(Wal::open(default_config(tmp.path())).expect("first open"));
    let before = segment_paths(tmp.path());
    assert_eq!(before.len(), 1, "first open created exactly one segment");
    let original_path = before.into_iter().next().expect("the one segment");
    let original_bytes = std::fs::read(&original_path).expect("read original");
    drop(Wal::open(default_config(tmp.path())).expect("second open"));
    let after = segment_paths(tmp.path());
    assert_eq!(
        after,
        vec![original_path.clone()],
        "second open MUST reuse the exact same segment file — not delete + recreate, not add a sibling",
    );
    // Header bytes unchanged: the second open doesn't
    // rewrite the segment header (no double-write that could
    // corrupt frames in a future slice).
    let after_bytes = std::fs::read(&original_path).expect("read after");
    assert_eq!(
        original_bytes, after_bytes,
        "segment contents are unchanged by reopen",
    );
}

/// `wal_root` does not have to pre-exist — `Wal::open`
/// recursively creates it (operator-friendly `mkdir -p`
/// semantics, matches the "single Rust binary, just point it
/// at a directory" §1 summary).
#[test]
fn missing_root_is_created_recursively() {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let nested = tmp.path().join("a/b/c");
    assert!(!nested.exists());
    let _wal = Wal::open(default_config(&nested)).expect("missing root opens");
    assert!(nested.is_dir(), "wal_root was created recursively");
}

/// Every §6.9 Tunable's out-of-range arm surfaces as
/// `InvalidConfig` naming the offending field. Iterated for
/// brevity rather than one test per arm.
#[test]
fn every_tunable_out_of_range_value_is_rejected() {
    type CaseBuilder = dyn Fn(&Path) -> WalConfig;
    let cases: &[(&str, &CaseBuilder)] = &[
        ("batch_window_ms", &|root| WalConfig {
            batch_window_ms: MAX_BATCH_WINDOW_MS + 1,
            ..default_config(root)
        }),
        ("segment_size_bytes", &|root| WalConfig {
            segment_size_bytes: MAX_SEGMENT_SIZE_BYTES + 1,
            ..default_config(root)
        }),
        ("segment_age_secs", &|root| WalConfig {
            segment_age_secs: MAX_SEGMENT_AGE_SECS + 1,
            ..default_config(root)
        }),
        ("segment_age_secs", &|root| WalConfig {
            segment_age_secs: MIN_SEGMENT_AGE_SECS - 1,
            ..default_config(root)
        }),
        ("housekeeping_secs", &|root| WalConfig {
            housekeeping_secs: MAX_HOUSEKEEPING_SECS + 1,
            ..default_config(root)
        }),
        ("housekeeping_secs", &|root| WalConfig {
            housekeeping_secs: MIN_HOUSEKEEPING_SECS - 1,
            ..default_config(root)
        }),
    ];
    for (field, make_cfg) in cases {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let err = Wal::open(make_cfg(tmp.path())).expect_err("must reject");
        match err {
            OpenError::InvalidConfig { field: f, .. } => assert_eq!(
                &f, field,
                "validation should name the violating field exactly",
            ),
            other => panic!("expected InvalidConfig({field}), got {other:?}"),
        }
    }
}

fn segment_paths(root: &Path) -> Vec<std::path::PathBuf> {
    let mut v: Vec<_> = std::fs::read_dir(root)
        .expect("read_dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "wal"))
        .collect();
    v.sort();
    v
}
