//! Conditional-PUT interop with an S3-compatible store that honours
//! `If-Match` only for the **unquoted** `ETag`. Ceph RGW-based stores do
//! this: a `GET` returns the `ETag` quoted, and an `If-Match` carrying that
//! quoted value gets `412` even when the object is unchanged. Every
//! compaction commit against such a store used to lose its
//! compare-and-swap, so the sweep rewrote each partition, discarded the
//! result as a "lost race", and reported a clean no-op forever (#807).
//!
//! The store is a minimal in-process S3 fake over `std::net` — `PUT`
//! (plain, `If-None-Match: *`, `If-Match`), `GET`, `DELETE` and
//! `ListObjectsV2` — enough for [`Store::s3`] and [`compact_partition`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use ourios_core::record::MinedRecord;
use ourios_parquet::compaction::visit_partition_rows;
use ourios_parquet::{Manifest, PartitionKey, S3Config, Store, Writer, compact_partition};

use super::rfc0013_object_store::rec_for;

const BUCKET: &str = "rgw";

/// Object key → (bytes, unquoted `ETag`).
type Objects = Arc<Mutex<BTreeMap<String, (Vec<u8>, String)>>>;

struct Request {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn query(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).expect("hex");
                out.push(u8::from_str_radix(hex, 16).expect("hex digit"));
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).expect("utf8")
}

fn read_request(stream: &TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        reader.read_line(&mut h).ok()?;
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        let (k, v) = h.split_once(':')?;
        headers.push((k.trim().to_string(), v.trim().to_string()));
    }
    let len = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .map_or(0, |(_, v)| v.parse::<usize>().expect("content-length"));
    let mut body = vec![0; len];
    reader.read_exact(&mut body).ok()?;
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    let query = query
        .split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect();
    Some(Request {
        method,
        path: percent_decode(path),
        query,
        headers,
        body,
    })
}

fn respond(mut stream: &TcpStream, status: &str, headers: &[(&str, String)], body: &[u8]) {
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        let _ = write!(head, "{k}: {v}\r\n");
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn list(objects: &Objects, req: &Request) -> String {
    let prefix = req.query("prefix").unwrap_or("");
    let delimiter = req.query("delimiter");
    let objects = objects.lock().unwrap_or_else(PoisonError::into_inner);
    let mut contents = String::new();
    let mut prefixes = BTreeSet::new();
    for (key, (bytes, etag)) in objects.range(prefix.to_string()..) {
        let Some(rest) = key.strip_prefix(prefix) else {
            break;
        };
        match delimiter.and_then(|d| rest.find(d).map(|i| (i, d))) {
            Some((i, d)) => {
                prefixes.insert(format!("{prefix}{}{d}", &rest[..i]));
            }
            None => {
                let _ = write!(
                    contents,
                    "<Contents><Key>{key}</Key><LastModified>2026-01-01T00:00:00.000Z</LastModified>\
                     <ETag>&quot;{etag}&quot;</ETag><Size>{}</Size></Contents>",
                    bytes.len()
                );
            }
        }
    }
    let common = prefixes.iter().fold(String::new(), |mut out, p| {
        let _ = write!(out, "<CommonPrefixes><Prefix>{p}</Prefix></CommonPrefixes>");
        out
    });
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult><Name>{BUCKET}</Name>\
         <Prefix>{prefix}</Prefix><IsTruncated>false</IsTruncated>{contents}{common}</ListBucketResult>"
    )
}

/// Which `If-Match` spelling of the current `ETag` a store honours.
#[derive(Clone, Copy)]
enum IfMatch {
    /// Only the unquoted form — the Ceph RGW quirk under test.
    Unquoted,
    /// Only the quoted `entity-tag` RFC 9110 §8.8.3 defines.
    Quoted,
    /// Either form, as lenient stores do.
    Either,
    /// Neither: every compare-and-swap loses.
    Never,
}

impl IfMatch {
    fn matches(self, expected: &str, etag: &str) -> bool {
        let quoted = expected.strip_prefix('"').and_then(|e| e.strip_suffix('"'));
        match (self, quoted) {
            // A bare `If-Match` is parsed as a server would: `*` matches any
            // existing object, and a comma separates a list of tags.
            (Self::Unquoted | Self::Either, None) => {
                expected == "*" || expected.split(',').any(|tag| tag.trim() == etag)
            }
            (Self::Quoted | Self::Either, Some(inner)) => inner == etag,
            (Self::Unquoted, Some(_)) | (Self::Quoted, None) | (Self::Never, _) => false,
        }
    }
}

/// `412` unless the conditional headers hold, comparing `If-Match` against
/// the current `ETag` as `mode` does.
fn precondition_holds(mode: IfMatch, req: &Request, current: Option<&String>) -> bool {
    match (req.header("if-none-match"), req.header("if-match"), current) {
        (_, Some(expected), Some(etag)) => mode.matches(expected, etag),
        (Some("*"), _, Some(_)) | (_, Some(_), None) => false,
        _ => true,
    }
}

/// The fake's per-store state; `if_match_puts` counts every `PUT` carrying
/// `If-Match`, so a test can see whether a retry was sent.
#[derive(Default)]
struct State {
    objects: Objects,
    next_etag: Mutex<u64>,
    if_match_puts: AtomicUsize,
    interleave: Mutex<Option<Interleave>>,
}

/// A change another process makes to the bucket, applied once, on the first
/// `GET` (a listing or an object read): after that request's answer is
/// computed and before it is sent, so the client sees the old state in that
/// answer and the new state in every later one.
type Interleave = Box<dyn FnOnce(&mut BTreeMap<String, (Vec<u8>, String)>) + Send>;

impl State {
    fn interleave_on_first_read(&self, change: Interleave) {
        *self
            .interleave
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(change);
    }

    fn run_interleave(&self) {
        let change = self
            .interleave
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(change) = change {
            change(&mut self.objects.lock().unwrap_or_else(PoisonError::into_inner));
        }
    }
}

fn get(state: &State, key: &str, stream: &TcpStream) {
    let found = state
        .objects
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(key)
        .cloned();
    state.run_interleave();
    match found {
        Some((bytes, etag)) => respond(
            stream,
            "200 OK",
            &[
                ("ETag", format!("\"{etag}\"")),
                ("Last-Modified", "Thu, 01 Jan 2026 00:00:00 GMT".to_string()),
            ],
            &bytes,
        ),
        None => respond(
            stream,
            "404 Not Found",
            &[],
            b"<Error><Code>NoSuchKey</Code></Error>",
        ),
    }
}

fn handle(mode: IfMatch, state: &State, stream: &TcpStream) {
    let Some(req) = read_request(stream) else {
        return;
    };
    let key = req
        .path
        .strip_prefix(&format!("/{BUCKET}"))
        .unwrap_or(&req.path)
        .trim_start_matches('/')
        .to_string();
    match req.method.as_str() {
        "GET" if key.is_empty() => list_objects(state, &req, stream),
        "GET" | "HEAD" => get(state, &key, stream),
        "PUT" => put(mode, state, req, key, stream),
        "DELETE" => delete(state, &key, stream),
        "POST" if req.query("delete").is_some() => bulk_delete(&state.objects, &req, stream),
        _ => respond(stream, "405 Method Not Allowed", &[], b""),
    }
}

/// `ListObjectsV2`, answered from the state before any interleaved change.
fn list_objects(state: &State, req: &Request, stream: &TcpStream) {
    let xml = list(&state.objects, req);
    state.run_interleave();
    respond(stream, "200 OK", &[], xml.as_bytes());
}

/// `PUT`, plain or conditional as `mode` compares tags.
fn put(mode: IfMatch, state: &State, req: Request, key: String, stream: &TcpStream) {
    if req.header("if-match").is_some() {
        state.if_match_puts.fetch_add(1, Ordering::SeqCst);
    }
    let Some(etag) = store_if_precondition_holds(mode, state, req, key) else {
        respond(
            stream,
            "412 Precondition Failed",
            &[],
            b"<Error><Code>PreconditionFailed</Code></Error>",
        );
        return;
    };
    respond(stream, "200 OK", &[("ETag", format!("\"{etag}\""))], b"");
}

/// Store `req`'s body at `key` under a fresh `ETag` when its conditional
/// headers hold; that `ETag`, or `None` for a failed precondition.
fn store_if_precondition_holds(
    mode: IfMatch,
    state: &State,
    req: Request,
    key: String,
) -> Option<String> {
    let mut objects = state.objects.lock().unwrap_or_else(PoisonError::into_inner);
    let current = objects.get(&key).map(|(_, etag)| etag);
    if !precondition_holds(mode, &req, current) {
        return None;
    }
    let mut n = state
        .next_etag
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    *n += 1;
    let etag = format!("e{n}");
    objects.insert(key, (req.body, etag.clone()));
    Some(etag)
}

fn delete(state: &State, key: &str, stream: &TcpStream) {
    state
        .objects
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(key);
    respond(stream, "204 No Content", &[], b"");
}

/// `DeleteObjects` (`POST ?delete`), which `object_store` uses for every
/// delete: removes each `<Key>` in the body and reports it deleted.
fn bulk_delete(objects: &Objects, req: &Request, stream: &TcpStream) {
    let body = String::from_utf8_lossy(&req.body);
    let keys: Vec<&str> = body
        .split("<Key>")
        .skip(1)
        .filter_map(|rest| rest.split_once("</Key>").map(|(key, _)| key))
        .collect();
    let mut objects = objects.lock().unwrap_or_else(PoisonError::into_inner);
    let deleted = keys.iter().fold(String::new(), |mut out, key| {
        objects.remove(*key);
        let _ = write!(out, "<Deleted><Key>{key}</Key></Deleted>");
        out
    });
    let xml =
        format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><DeleteResult>{deleted}</DeleteResult>");
    respond(stream, "200 OK", &[], xml.as_bytes());
}

/// Start the fake honouring `mode` and return a [`Store`] pointed at it,
/// with the fake's state for inspection.
fn fake_store(mode: IfMatch) -> (Store, Arc<State>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let state = Arc::new(State::default());
    let served = Arc::clone(&state);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let served = Arc::clone(&served);
            std::thread::spawn(move || handle(mode, &served, &stream));
        }
    });
    let store = Store::s3(
        S3Config::new(BUCKET)
            .with_endpoint(format!("http://{addr}"))
            .with_region("us-east-1")
            .with_access_key_id("test")
            .with_secret_access_key("test"),
    )
    .expect("build s3 store");
    (store, state)
}

fn unquoted_etag_store() -> Store {
    fake_store(IfMatch::Unquoted).0
}

/// Put `v1` then `v2` at `key`, returning the `ETag` `v1` had.
fn stale_etag(store: &Store, key: &str) -> String {
    store.put_blocking(key, b"v1".to_vec()).expect("put");
    let (_, stale) = store
        .get_with_etag_blocking_opt(key)
        .expect("get")
        .expect("present");
    store
        .put_blocking(key, b"v2".to_vec())
        .expect("concurrent writer");
    stale.expect("etag")
}

/// A compare-and-swap against the `ETag` the store itself just returned
/// must win, whichever `ETag` spelling the store honours.
#[test]
fn put_if_match_wins_with_the_current_etag() {
    let store = unquoted_etag_store();
    store
        .put_blocking("data/manifest.json", b"v1".to_vec())
        .expect("put");
    let (_, etag) = store
        .get_with_etag_blocking_opt("data/manifest.json")
        .expect("get")
        .expect("present");
    let etag = etag.expect("etag");

    store
        .put_if_match_blocking("data/manifest.json", b"v2".to_vec(), &etag)
        .expect("CAS against the current ETag wins");

    assert_eq!(
        store.get_blocking("data/manifest.json").expect("get"),
        b"v2".to_vec()
    );
}

/// The retry never weakens the swap: a stale `ETag` still loses, in both
/// spellings, and the object keeps the winner's bytes.
#[test]
fn put_if_match_still_loses_with_a_stale_etag() {
    let store = unquoted_etag_store();
    store
        .put_blocking("data/manifest.json", b"v1".to_vec())
        .expect("put");
    let (_, stale) = store
        .get_with_etag_blocking_opt("data/manifest.json")
        .expect("get")
        .expect("present");
    let stale = stale.expect("etag");
    store
        .put_blocking("data/manifest.json", b"v2".to_vec())
        .expect("concurrent writer");

    for etag in [stale.clone(), stale.trim_matches('"').to_string()] {
        let err = store
            .put_if_match_blocking("data/manifest.json", b"lost".to_vec(), &etag)
            .expect_err("a stale ETag must lose");
        assert!(err.is_precondition(), "{err}");
    }
    assert_eq!(
        store.get_blocking("data/manifest.json").expect("get"),
        b"v2".to_vec()
    );
}

/// #807: a sealed two-file partition on such a store consolidates — the
/// commit wins instead of being discarded as a lost race.
#[test]
fn compaction_commits_on_an_unquoted_etag_store() {
    let store = unquoted_etag_store();
    let partition = PartitionKey::derive(&rec_for("tenant-a", 0)).expect("derive");
    for i in 0..2 {
        let mut writer = Writer::open_in(&store, partition.clone()).expect("open writer");
        writer
            .append_records(&[rec_for("tenant-a", i)])
            .expect("append");
        writer.close().expect("close");
    }

    let outcome = compact_partition(&store, &partition).expect("compact");

    let committed = outcome
        .committed
        .expect("the manifest swap commits instead of losing its CAS");
    assert_eq!(committed.input_files.len(), 2);
    assert_eq!(outcome.rows, 2);
}

/// A store that honours the quoted `ETag` is unaffected: the first swap wins
/// and no unquoted retry is sent.
#[test]
fn put_if_match_sends_one_request_when_the_quoted_etag_wins() {
    for mode in [IfMatch::Quoted, IfMatch::Either] {
        let (store, state) = fake_store(mode);
        store
            .put_blocking("data/manifest.json", b"v1".to_vec())
            .expect("put");
        let (_, etag) = store
            .get_with_etag_blocking_opt("data/manifest.json")
            .expect("get")
            .expect("present");
        let etag = etag.expect("etag");
        assert!(etag.starts_with('"') && etag.ends_with('"'), "{etag}");

        store
            .put_if_match_blocking("data/manifest.json", b"v2".to_vec(), &etag)
            .expect("CAS against the current ETag wins");

        assert_eq!(state.if_match_puts.load(Ordering::SeqCst), 1);
        assert_eq!(
            store.get_blocking("data/manifest.json").expect("get"),
            b"v2".to_vec()
        );
    }
}

/// Whichever spelling a store honours, a stale `ETag` loses in both
/// spellings — the retry never lets a stale writer win.
#[test]
fn put_if_match_loses_with_a_stale_etag_on_every_store() {
    for mode in [IfMatch::Unquoted, IfMatch::Quoted, IfMatch::Either] {
        let (store, _) = fake_store(mode);
        let stale = stale_etag(&store, "data/manifest.json");

        for etag in [stale.clone(), stale.trim_matches('"').to_string()] {
            let err = store
                .put_if_match_blocking("data/manifest.json", b"lost".to_vec(), &etag)
                .expect_err("a stale ETag must lose");
            assert!(err.is_precondition(), "{err}");
        }
        assert_eq!(
            store.get_blocking("data/manifest.json").expect("get"),
            b"v2".to_vec()
        );
    }
}

/// A rewrite whose final manifest swap loses reports `commit_lost` and
/// removes its consolidated object: no manifest names it, and an erasure
/// retrying the partition never runs `gc_orphans` on it.
#[test]
fn a_lost_final_swap_removes_its_rewrite() {
    let (store, _) = fake_store(IfMatch::Never);
    let partition = PartitionKey::derive(&rec_for("tenant-a", 0)).expect("derive");
    for i in 0..2 {
        let mut writer = Writer::open_in(&store, partition.clone()).expect("open writer");
        writer
            .append_records(&[rec_for("tenant-a", i)])
            .expect("append");
        writer.close().expect("close");
    }
    // An existing manifest makes the commit an `If-Match` swap, which this
    // store never honours.
    let manifest_key = format!("{}/manifest.json", name_prefix(&partition));
    let existing = Manifest::new(1, Vec::new(), Vec::new());
    store
        .put_blocking(&manifest_key, existing.to_json().expect("json"))
        .expect("seed manifest");
    let parquet_keys = || -> Vec<String> {
        let mut keys: Vec<_> = store
            .list_blocking(Some("data/"))
            .expect("list")
            .into_iter()
            .filter(|k| k.ends_with(".parquet"))
            .collect();
        keys.sort();
        keys
    };
    let inputs = parquet_keys();

    let outcome = compact_partition(&store, &partition).expect("compact");

    assert_eq!(
        (
            outcome.committed.is_none(),
            outcome.commit_lost,
            outcome.gc_failures
        ),
        (true, true, 0)
    );
    assert_eq!(parquet_keys(), inputs, "only the inputs remain");
}

/// The unquoted retry never turns one strong comparison into a wildcard or
/// a list: a stale tag whose unquoted spelling is `*`, or a list naming the
/// current tag, still loses and the object keeps its bytes. Each case gets
/// its own store, so one wrongly accepted write cannot mask the next.
#[test]
fn put_if_match_never_retries_a_tag_that_would_change_meaning_unquoted() {
    let wildcard = |_: &str| "\"*\"".to_string();
    let listed = |current: &str| format!("\"stale,{}\"", current.trim_matches('"'));

    let kept: Vec<Vec<u8>> = [&wildcard as &dyn Fn(&str) -> String, &listed]
        .iter()
        .map(|spell| {
            let (store, _) = fake_store(IfMatch::Unquoted);
            store
                .put_blocking("data/manifest.json", b"v1".to_vec())
                .expect("put");
            let (_, current) = store
                .get_with_etag_blocking_opt("data/manifest.json")
                .expect("get")
                .expect("present");
            let etag = spell(&current.expect("etag"));
            let err = store
                .put_if_match_blocking("data/manifest.json", b"lost".to_vec(), &etag)
                .expect_err("a tag that changes meaning unquoted must lose");
            assert!(err.is_precondition(), "{etag}: {err}");
            store.get_blocking("data/manifest.json").expect("get")
        })
        .collect();

    assert_eq!(kept, [b"v1".to_vec(), b"v1".to_vec()]);
}

/// Swap `key` to `bytes` against its current `ETag`, as the store returns it.
fn swap(store: &Store, key: &str, bytes: &[u8]) {
    let (_, etag) = store
        .get_with_etag_blocking_opt(key)
        .expect("get")
        .expect("present");
    store
        .put_if_match_blocking(key, bytes.to_vec(), &etag.expect("etag"))
        .expect("CAS against the current ETag wins");
}

/// Once a swap has won, the next one — from any handle on the backend —
/// leads with the spelling that won and sends one `PUT`, not a `412` and a
/// retry; and a stale `ETag` still loses afterwards.
#[test]
fn put_if_match_leads_with_the_spelling_that_last_won() {
    for mode in [IfMatch::Unquoted, IfMatch::Quoted, IfMatch::Either] {
        let (store, state) = fake_store(mode);
        store
            .put_blocking("data/manifest.json", b"v1".to_vec())
            .expect("put");
        swap(&store, "data/manifest.json", b"v2");
        state.if_match_puts.store(0, Ordering::SeqCst);

        swap(&store.clone(), "data/manifest.json", b"v3");

        assert_eq!(state.if_match_puts.load(Ordering::SeqCst), 1);
        let stale_tag = stale_etag(&store, "data/manifest.json");
        let err = store
            .put_if_match_blocking("data/manifest.json", b"lost".to_vec(), &stale_tag)
            .expect_err("a stale ETag must lose");
        assert!(err.is_precondition(), "{err}");
        assert_eq!(
            store.get_blocking("data/manifest.json").expect("get"),
            b"v2".to_vec()
        );
    }
}

/// The bytes of one committed Parquet file holding `rows`, written through a
/// throwaway local store, for a test to place in the fake bucket.
fn parquet_bytes(partition: &PartitionKey, rows: &[MinedRecord]) -> Vec<u8> {
    let dir = tempfile::tempdir().expect("temp");
    let local = Store::local(dir.path()).expect("local store");
    let mut writer = Writer::open_in(&local, partition.clone()).expect("open writer");
    writer.append_records(rows).expect("append");
    let key = writer.close().expect("close").key;
    local.get_blocking(&key).expect("get")
}

/// Write each of `rows` as its own flush file; their keys.
fn write_flush_files(store: &Store, partition: &PartitionKey, rows: &[MinedRecord]) -> Vec<String> {
    rows.iter()
        .map(|row| {
            let mut writer = Writer::open_in(store, partition.clone()).expect("open writer");
            writer
                .append_records(std::slice::from_ref(row))
                .expect("append");
            writer.close().expect("close").key
        })
        .collect()
}

/// The `time_unix_nano` of every row `visit_partition_rows` yields, sorted.
fn visited_times(store: &Store, partition: &PartitionKey) -> Vec<u64> {
    let mut times = Vec::new();
    visit_partition_rows(store, partition, |batch| {
        times.extend(batch.iter().map(|r| r.time_unix_nano));
    })
    .expect("visit");
    times.sort_unstable();
    times
}

fn name_of(key: &str) -> String {
    key.rsplit_once('/').expect("partition key").1.to_owned()
}

fn name_prefix(partition: &PartitionKey) -> String {
    partition
        .data_path(std::path::Path::new(""))
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

/// A peer's commit at generation 2: its output `out` holding `rows`, and a
/// manifest naming it and consuming `consumed`, as objects to add.
fn peer_commit(
    partition: &PartitionKey,
    out: &str,
    rows: &[MinedRecord],
    consumed: Vec<String>,
) -> BTreeMap<String, (Vec<u8>, String)> {
    let prefix = name_prefix(partition);
    let manifest = Manifest::new(2, vec![out.to_owned()], consumed);
    BTreeMap::from([
        (
            format!("{prefix}/{out}"),
            (parquet_bytes(partition, rows), "peer-out".to_owned()),
        ),
        (
            format!("{prefix}/manifest.json"),
            (
                manifest.to_json().expect("json"),
                "peer-manifest".to_owned(),
            ),
        ),
    ])
}

/// RFC0009.18 (compactor read path): a partition's first commit lands between
/// `visit_partition_rows`' listing and its manifest read, and deletes the
/// inputs. The visit sees that commit through the manifest and reads every row
/// once, from the compacted file it names.
#[test]
fn visit_reads_every_row_across_a_first_commit_between_its_reads() {
    let (store, state) = fake_store(IfMatch::Either);
    let partition = PartitionKey::derive(&rec_for("tenant-a", 0)).expect("derive");
    let rows = [rec_for("tenant-a", 0), rec_for("tenant-a", 1)];
    let inputs = write_flush_files(&store, &partition, &rows);
    let out = "00000000-0000-7000-8000-000000000001.g2.parquet";
    let consumed = inputs.iter().map(|k| name_of(k)).collect();
    let commit = peer_commit(&partition, out, &rows, consumed);
    state.interleave_on_first_read(Box::new(move |objects| {
        objects.extend(commit);
        for input in &inputs {
            objects.remove(input);
        }
    }));

    let times = visited_times(&store, &partition);

    assert_eq!(times, rows.map(|r| r.time_unix_nano), "every row, once");
}
