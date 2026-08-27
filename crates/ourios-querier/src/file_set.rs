//! Live-file resolution (local walk + manifest, S3 twin) and the
//! `DataFusion` listing-URL construction — one job: which bytes may this
//! query read (epic #745 wave 1; moved verbatim from the crate root).

// Split from the crate root (epic #745 wave 1); the parent scope is
// the import surface so every pre-split `crate::X` path resolves
// unchanged.
#[allow(clippy::wildcard_imports)]
use super::*;

/// Resolve the live data files a query must read under `dir` (a
/// tenant's partition root), honouring the RFC 0009 §3.4
/// per-partition manifest. Recursive because the data is nested
/// `year=/month=/day=/hour=/`.
///
/// For each partition directory: if it holds a `manifest.json`, the
/// manifest is authoritative and contributes exactly the files it
/// names (files present on disk but not listed — orphans awaiting GC,
/// or a writer's uncommitted `*.parquet.tmp` — are ignored). With no
/// manifest (every partition today, pre-compaction) it falls back to
/// all committed `*.parquet` in that directory; `*.parquet.tmp` has
/// extension `tmp`, so the poisoned-writer case contributes nothing.
///
/// An empty result means the tenant has nothing queryable. A missing
/// directory (`NotFound`) is empty; any *other* I/O error (permission
/// denied, transient failure) is propagated as [`QueryError::Storage`]
/// rather than silently masked as "no data" — a wrong zero-row answer
/// is worse than a surfaced error.
pub(super) fn resolve_live_files(
    dir: &std::path::Path,
    window: Option<(u64, u64)>,
) -> Result<Vec<PathBuf>, QueryError> {
    let io_err = |op: &str, p: &std::path::Path, e: &std::io::Error| QueryError::Storage {
        detail: format!("{op} {}: {e}", p.display()),
    };
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let entries = match std::fs::read_dir(&d) {
            Ok(entries) => entries,
            // The dir (or a subdir, lost to a concurrent housekeeping
            // unlink) simply isn't there → not data, not an error.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(io_err("read_dir", &d, &e)),
        };
        let mut subdirs = Vec::new();
        let mut parquets = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| io_err("read_dir entry", &d, &e))?;
            let path = entry.path();
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => subdirs.push(path),
                Ok(_) if path.extension().is_some_and(|x| x == "parquet") => parquets.push(path),
                Ok(_) => {}
                Err(e) => return Err(io_err("file_type", &path, &e)),
            }
        }
        // Partition-level time pruning (RFC 0007): when the query has a
        // time range, skip a leaf partition whose `hour=HH` span can't
        // overlap it — so DataFusion never opens those footers. This is
        // a pure optimisation layered on the row-level time column
        // predicate (which stays the correctness authority);
        // `hour_partition_in_window` is conservative, never pruning a
        // path it can't prove out of range, so no in-window data is lost.
        let keep = window.is_none_or(|(start, end)| hour_partition_in_window(&d, start, end));
        if keep {
            match Manifest::read(&d).map_err(|e| QueryError::Storage {
                detail: format!("manifest in {}: {e}", d.display()),
            })? {
                // Manifest is authoritative: only its named files are live.
                Some(manifest) => {
                    files.extend(manifest.files.into_iter().map(|name| d.join(name)));
                }
                // No manifest → glob fallback for this partition.
                None => files.append(&mut parquets),
            }
        }
        stack.extend(subdirs);
    }
    Ok(files)
}

/// The S3 analog of [`resolve_live_files`]: resolve the live data-file **keys**
/// under the tenant's `prefix` through the [`Store`] seam (RFC 0019 §3.3),
/// honouring partition-level time pruning + the RFC 0009 §3.4 per-partition
/// manifest. Returns store-relative keys (the same key space `Store::get`/`put`
/// take), addressed as object-store URLs by the caller.
///
/// [`Store::list_blocking`] returns every key under `prefix` recursively, in
/// lexicographic order, segment-wise prefix-scoped to this tenant (RFC0019.5).
/// The keys are grouped by their partition directory (everything up to the last
/// `/`); for each partition: skip it when an `hour=HH` window prune proves it
/// out of range, then if it carries a `manifest.json` the manifest is
/// authoritative (only its named files are live, joined onto the partition key),
/// otherwise fall back to the partition's committed `*.parquet` keys
/// (`*.parquet.tmp` is excluded — it does not end in `.parquet`).
pub(super) fn resolve_live_keys(
    store: &Store,
    prefix: &str,
    window: Option<(u64, u64)>,
) -> Result<Vec<String>, QueryError> {
    let keys = store
        .list_blocking(Some(prefix))
        .map_err(|e| QueryError::Storage {
            detail: format!("list data prefix {prefix}: {e}"),
        })?;
    // Group keys by partition directory (the key up to its last `/`).
    let mut by_partition: std::collections::BTreeMap<&str, Vec<&str>> =
        std::collections::BTreeMap::new();
    for key in &keys {
        let (dir, _) = key.rsplit_once('/').unwrap_or(("", key.as_str()));
        by_partition.entry(dir).or_default().push(key);
    }

    let mut live = Vec::new();
    for (dir, partition_keys) in by_partition {
        // Partition-level time pruning (RFC 0007), conservative — never prunes a
        // partition it can't prove out of range. `hour_partition_in_window`
        // parses the trailing Hive segments off a path, so build one from the
        // partition-dir key.
        if let Some((start, end)) = window
            && !hour_partition_in_window(&PathBuf::from(dir), start, end)
        {
            continue;
        }
        let manifest_key = format!("{dir}/{MANIFEST_FILENAME}");
        // Only read the manifest when its key is actually in the listing: the
        // partition is already enumerated, so a `read_with_etag` for an absent
        // manifest is a wasted (404) GET per un-compacted partition on S3.
        // Absent ⇒ no manifest ⇒ all committed files live (same as today's
        // glob fallback). `list_blocking` returns store-relative keys, so this
        // compares like-for-like.
        let manifest = if partition_keys.iter().any(|k| *k == manifest_key) {
            Manifest::read_with_etag(store, &manifest_key).map_err(|e| QueryError::Storage {
                detail: format!("manifest {manifest_key}: {e}"),
            })?
        } else {
            None
        };
        match manifest {
            // Manifest is authoritative: only its named files are live (joined
            // onto the partition key as `<dir>/<name>`).
            Some((manifest, _etag)) => {
                live.extend(
                    manifest
                        .files
                        .into_iter()
                        .map(|name| format!("{dir}/{name}")),
                );
            }
            // No manifest → glob fallback for this partition's committed files.
            None => live.extend(
                partition_keys
                    .into_iter()
                    .filter(|k| k.ends_with(".parquet"))
                    .map(ToOwned::to_owned),
            ),
        }
    }
    Ok(live)
}

/// Build the `DataFusion` table URLs for the **local** backend: every resolved
/// file must canonicalize *under* the tenant's canonical partition root before
/// it is addressed, the tenant-isolation backstop (RFC0007.5 / §3.7). The
/// manifest's entries are already validated as partition-local names
/// (`Manifest::validate`), but a symlinked `*.parquet` could still resolve
/// outside — this `starts_with` check fails such a path loudly rather than
/// reading another tenant's data. Canonical paths are de-duplicated so a
/// manifest naming the same file twice can't double-count its rows.
///
/// Each URL is the canonical absolute path: `DataFusion` 53 treats an absolute
/// filesystem path as local and URI-encodes it internally, so spaces / reserved
/// characters are handled without a hand-built `file://…` string.
/// `year/month/day/hour` stay path-only (not file columns) and the query
/// filters only data columns, so no table partition columns are declared.
pub(super) fn local_file_urls(
    tenant_dir: &std::path::Path,
    live_files: &[PathBuf],
) -> Result<Vec<ListingTableUrl>, QueryError> {
    if live_files.is_empty() {
        return Ok(Vec::new());
    }
    let tenant_root = tenant_dir.canonicalize().map_err(|e| QueryError::Storage {
        detail: format!("canonicalize {}: {e}", tenant_dir.display()),
    })?;
    let mut seen = std::collections::HashSet::new();
    let mut urls = Vec::with_capacity(live_files.len());
    for file in live_files {
        let abs = file.canonicalize().map_err(|e| QueryError::Storage {
            detail: format!("canonicalize {}: {e}", file.display()),
        })?;
        if !abs.starts_with(&tenant_root) {
            return Err(QueryError::Storage {
                detail: format!(
                    "resolved file {} escapes tenant partition root {}",
                    abs.display(),
                    tenant_root.display(),
                ),
            });
        }
        if seen.insert(abs.clone()) {
            urls.push(ListingTableUrl::parse(abs.display().to_string()).map_err(storage_err)?);
        }
    }
    Ok(urls)
}

/// Build the `DataFusion` table URLs for the **S3** backend: register the
/// [`Store`]'s `object_store` on `ctx` under the [`STORE_URL`] scheme/authority
/// and address each store-relative key by an `ourios://store/<key>` URL
/// (RFC 0019 §3.3). Tenant isolation is the segment-wise prefix scope of the
/// listing that produced `keys` (RFC0019.5) — the object key space has no
/// symlinks, so there is no canonical-path escape to backstop here (the §3.7
/// row-level backstop in the consumers stays). De-duplicates keys so a manifest
/// naming the same file twice can't double-count its rows.
pub(super) fn object_store_urls(
    ctx: &SessionContext,
    store: &Store,
    keys: &[String],
) -> Result<Vec<ListingTableUrl>, QueryError> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let store_url = datafusion::execution::object_store::ObjectStoreUrl::parse(STORE_URL)
        .map_err(storage_err)?;
    ctx.register_object_store(store_url.as_ref(), store.object_store());
    // `Store::object_store()` is the RAW backend (prefix NOT applied), whereas
    // `list_blocking`/`get_blocking` operate in the store-relative key space
    // under `Store::prefix()` (the `OURIOS_S3_PREFIX` root). So the URLs handed
    // to DataFusion — which reads the raw backend directly — must carry the FULL
    // key: the store prefix segments followed by the relative key. With no
    // prefix (the local default) this is just the key.
    let prefix: Vec<String> = store
        .prefix()
        .parts()
        .map(|p| p.as_ref().to_owned())
        .collect();
    let mut seen = std::collections::HashSet::new();
    let mut urls = Vec::with_capacity(keys.len());
    for key in keys {
        if seen.insert(key.clone()) {
            urls.push(
                ListingTableUrl::parse(object_store_url_for_key(&prefix, key))
                    .map_err(storage_err)?,
            );
        }
    }
    Ok(urls)
}

/// Build the `ourios://store/<prefix>/<key>` URL for a store-relative `key`
/// under the store's `prefix` segments, percent-encoding each path segment.
///
/// Two reasons the full path matters:
/// - **Prefix** — `Store::object_store()` is the un-scoped raw backend, so the
///   URL must carry the store's `OURIOS_S3_PREFIX` root (`prefix`) ahead of the
///   relative key, or `DataFusion` would address an un-prefixed (not-found) path.
/// - **Encoding** — `ListingTableUrl::parse` URL-**decodes** the path, and a
///   key carries literal `%` (the partition dir is `tenant_id=<percent-encoded>`,
///   e.g. `tenant_id=tenant%20ABC`), so an un-encoded segment would be
///   double-decoded into a wrong path. Encoding every non-unreserved byte per
///   segment (and re-joining with `/`) makes the parse round-trip back to the
///   exact full key. `NON_ALPHANUMERIC` over-encodes harmlessly (`=`, `-`, `.`
///   round-trip the same); the only structural byte we keep is the `/`
///   separator, preserved by the per-segment split.
pub(super) fn object_store_url_for_key(prefix: &[String], key: &str) -> String {
    use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
    let encode = |segment: &str| utf8_percent_encode(segment, NON_ALPHANUMERIC).to_string();
    let encoded = prefix
        .iter()
        .map(|p| encode(p))
        .chain(key.split('/').map(encode))
        .collect::<Vec<_>>()
        .join("/");
    format!("{STORE_URL}/{encoded}")
}

/// Build the `DataFusion` table URLs for an **audit** scan (the drift query's
/// `ListingTable` over the audit stream) from a resolved [`AuditFiles`],
/// branching the same way as the bulk log scan (RFC 0019 §3.3):
///
/// - **Local** ([`AuditFiles::Local`]): the paths are already the
///   canonicalizing `std::fs` walk's output — absolute, canonical, deduped, and
///   tenant-isolation-checked (the symlink-escape / tenant-root backstops live
///   in [`audit_scan`]). Address each by its absolute local path.
/// - **S3** ([`AuditFiles::Remote`]): register the store on `ctx` and address
///   each key by its percent-encoded `ourios://store/<key>` object-store URL;
///   tenant isolation is the segment-wise prefix scope (RFC0019.5).
pub(crate) fn audit_table_urls(
    ctx: &SessionContext,
    backend: StoreRef<'_>,
    files: &audit_scan::AuditFiles,
) -> Result<Vec<ListingTableUrl>, QueryError> {
    match files {
        // The walk already produced absolute canonical paths, so address them
        // directly — no `root.join`, no CWD-relative path. The local branch
        // needs no `Store`.
        audit_scan::AuditFiles::Local(paths) => paths
            .iter()
            .map(|path| ListingTableUrl::parse(path.display().to_string()).map_err(storage_err))
            .collect(),
        // Remote keys imply the S3 backend, so `backend` is `Remote` here (it is
        // what produced these keys); a `Local` is an internal invariant
        // violation, surfaced rather than unwrapped (no panics, `CLAUDE.md` §6).
        audit_scan::AuditFiles::Remote(keys) => {
            let StoreRef::Remote(store) = backend else {
                return Err(QueryError::Storage {
                    detail: "internal: S3 audit URLs reached with a local backend".to_string(),
                });
            };
            object_store_urls(ctx, store, keys)
        }
    }
}

#[cfg(test)]
mod tests {
    #[allow(clippy::wildcard_imports)]
    use super::super::*;

    /// The S3 object-store URL for a key prepends the store prefix and
    /// percent-encodes every segment, so `ListingTableUrl::parse`'s URL-decode
    /// round-trips back to the **full** key the raw backend expects
    /// (`OURIOS_S3_PREFIX` + the store-relative key). The partition dir carries
    /// a literal `%` (`tenant_id=tenant%20ABC`) that must survive the parse.
    #[test]
    fn object_store_url_prepends_prefix_and_round_trips() {
        let prefix = vec!["ourios".to_string()];
        let key = "data/tenant_id=tenant%20ABC/year=2026/h.parquet";
        let url = object_store_url_for_key(&prefix, key);
        // The parsed URL's object-store path must decode back to prefix + key,
        // not double-decode the literal `%20` into a space.
        let parsed = ListingTableUrl::parse(&url).expect("parse url");
        let decoded = percent_encoding::percent_decode_str(parsed.as_ref())
            .decode_utf8()
            .expect("utf8");
        assert!(
            decoded.ends_with("ourios/data/tenant_id=tenant%20ABC/year=2026/h.parquet"),
            "decoded URL must carry the full prefixed key verbatim: {decoded}",
        );
    }

    /// With no store prefix (the local default), the URL is just the key —
    /// the prefix prepend is a no-op.
    #[test]
    fn object_store_url_with_no_prefix_is_just_the_key() {
        let url = object_store_url_for_key(&[], "data/tenant_id=t/h.parquet");
        let parsed = ListingTableUrl::parse(&url).expect("parse url");
        let decoded = percent_encoding::percent_decode_str(parsed.as_ref())
            .decode_utf8()
            .expect("utf8");
        assert!(
            decoded.ends_with("data/tenant_id=t/h.parquet"),
            "no-prefix URL is the bare key: {decoded}",
        );
    }

    /// Create `<root>/data/tenant_id=a/year=2026/.../hour=10` and
    /// return `(tenant_dir, partition_dir)`.
    fn tenant_and_partition(root: &std::path::Path) -> (PathBuf, PathBuf) {
        let tenant = root.join("data/tenant_id=a");
        let partition = tenant.join("year=2026/month=04/day=02/hour=10");
        std::fs::create_dir_all(&partition).expect("mkdir partition");
        (tenant, partition)
    }

    #[test]
    fn resolve_missing_tenant_dir_is_empty() {
        // Arrange — a tenant directory that was never written.
        let tmp = tempfile::tempdir().expect("temp");
        let ghost = tmp.path().join("data/tenant_id=ghost");

        // Act
        let files = resolve_live_files(&ghost, None).expect("resolve");

        // Assert
        assert!(files.is_empty());
    }

    #[test]
    fn resolve_tmp_only_partition_is_empty() {
        // Arrange — a partition holding only an uncommitted `.tmp`.
        let tmp = tempfile::tempdir().expect("temp");
        let (tenant, partition) = tenant_and_partition(tmp.path());
        std::fs::write(partition.join("x.parquet.tmp"), b"partial").expect("write tmp");

        // Act
        let files = resolve_live_files(&tenant, None).expect("resolve");

        // Assert
        assert!(files.is_empty(), "uncommitted .tmp files are not live");
    }

    #[test]
    fn resolve_globs_committed_parquet_without_a_manifest() {
        // Arrange — two committed files, no manifest.
        let tmp = tempfile::tempdir().expect("temp");
        let (tenant, partition) = tenant_and_partition(tmp.path());
        std::fs::write(partition.join("a.parquet"), b"a").expect("write a");
        std::fs::write(partition.join("b.parquet"), b"b").expect("write b");

        // Act
        let files = resolve_live_files(&tenant, None).expect("resolve");

        // Assert
        assert_eq!(
            files.len(),
            2,
            "both committed files are live without a manifest"
        );
    }

    #[test]
    fn resolve_manifest_is_authoritative() {
        // Arrange — two files on disk, a manifest naming only one.
        let tmp = tempfile::tempdir().expect("temp");
        let (tenant, partition) = tenant_and_partition(tmp.path());
        std::fs::write(partition.join("a.parquet"), b"a").expect("write a");
        std::fs::write(partition.join("b.parquet"), b"b").expect("write b");
        let manifest = ourios_parquet::Manifest {
            generation: 1,
            files: vec!["a.parquet".to_string()],
        };
        std::fs::write(
            partition.join(ourios_parquet::MANIFEST_FILENAME),
            manifest.to_json().unwrap(),
        )
        .expect("write manifest");

        // Act
        let files = resolve_live_files(&tenant, None).expect("resolve");

        // Assert
        assert_eq!(files.len(), 1, "only the manifest's file is live");
        assert!(files[0].ends_with("a.parquet"));
    }

    #[test]
    fn resolve_malformed_manifest_is_a_storage_error() {
        // Arrange — a manifest that isn't valid JSON.
        let tmp = tempfile::tempdir().expect("temp");
        let (tenant, partition) = tenant_and_partition(tmp.path());
        std::fs::write(partition.join("a.parquet"), b"a").expect("write a");
        std::fs::write(
            partition.join(ourios_parquet::MANIFEST_FILENAME),
            b"not json",
        )
        .expect("write manifest");

        // Act
        let result = resolve_live_files(&tenant, None);

        // Assert
        assert!(matches!(result, Err(QueryError::Storage { .. })));
    }
}
