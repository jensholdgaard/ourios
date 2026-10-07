//! Bounded-concurrency, order-preserving object fetch for the remote audit
//! scan (#853).
//!
//! A cold template-map fold GETs every audit object of the tenant. Issued one
//! at a time, each pays a full object-store round trip, so the fold's wall
//! time is the sum of the latencies. [`fetch_in_order`] keeps several GETs in
//! flight but hands their bodies to the consumer strictly in listing order,
//! so the fold sees exactly the sequence the serial scan saw.
//!
//! Two limits bound it. `requests` caps the GETs in flight. `bytes` caps the
//! listed size of every object that has been dispatched but not yet consumed —
//! in flight, or fetched and waiting for an earlier one — so out-of-order
//! completions cannot pile up behind a slow GET. An object larger than the
//! whole budget is still fetched, alone. The consumer decodes one object at a
//! time, as the serial scan did, so the decoded-events footprint is unchanged.

use std::collections::BTreeMap;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

use crate::QueryError;

/// The concurrency and memory limits of one [`fetch_in_order`] pass.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FetchLimits {
    /// GETs in flight at once (at least one is always allowed).
    pub(crate) requests: usize,
    /// Listed bytes dispatched but not yet consumed.
    pub(crate) bytes: u64,
}

impl Default for FetchLimits {
    fn default() -> Self {
        Self {
            requests: 16,
            bytes: 32 * 1024 * 1024,
        }
    }
}

#[derive(Default)]
struct Window {
    /// The next object index to dispatch; every index below it is
    /// dispatched, so the consumer's next object always is.
    next: usize,
    /// Listed bytes of dispatched, not yet consumed objects.
    reserved: u64,
    ready: BTreeMap<usize, Result<Vec<u8>, QueryError>>,
    /// The consumer has stopped (done or failed): dispatch nothing more.
    stopped: bool,
}

struct Shared {
    window: Mutex<Window>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Window> {
        self.window.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wait<'a>(&self, guard: MutexGuard<'a, Window>) -> MutexGuard<'a, Window> {
        self.changed
            .wait(guard)
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn complete(&self, index: usize, result: Result<Vec<u8>, QueryError>) {
        self.lock().ready.insert(index, result);
        self.changed.notify_all();
    }

    fn stop(&self) {
        self.lock().stopped = true;
        self.changed.notify_all();
    }

    /// Claim the next object to fetch, waiting while the byte budget is
    /// spent; `None` once everything is dispatched or the consumer stopped.
    fn claim(&self, sizes: &[u64], budget: u64) -> Option<usize> {
        let mut window = self.lock();
        loop {
            if window.stopped || window.next == sizes.len() {
                return None;
            }
            let size = sizes[window.next];
            if window.reserved == 0 || window.reserved.saturating_add(size) <= budget {
                let index = window.next;
                window.next += 1;
                window.reserved = window.reserved.saturating_add(size);
                return Some(index);
            }
            window = self.wait(window);
        }
    }

    fn take(&self, index: usize) -> Result<Vec<u8>, QueryError> {
        let mut window = self.lock();
        loop {
            if let Some(result) = window.ready.remove(&index) {
                return result;
            }
            window = self.wait(window);
        }
    }

    fn release(&self, size: u64) {
        let mut window = self.lock();
        window.reserved = window.reserved.saturating_sub(size);
        drop(window);
        self.changed.notify_all();
    }
}

/// Completes a claimed object even if its fetch panics, so the consumer
/// waiting on it gets an error instead of waiting forever.
struct Claim<'a> {
    shared: &'a Shared,
    index: usize,
    done: bool,
}

impl Claim<'_> {
    fn complete(mut self, result: Result<Vec<u8>, QueryError>) {
        self.done = true;
        self.shared.complete(self.index, result);
    }
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.shared.complete(
                self.index,
                Err(QueryError::Storage {
                    detail: "audit object fetch panicked".to_owned(),
                }),
            );
        }
    }
}

/// Fetch every `(key, listed size)` object with `fetch`, at most
/// `limits.requests` at once, and pass each body to `consume` in `objects`
/// order with its index. Stops at the first error — a failed GET or a failed
/// `consume` — and returns it; GETs already in flight finish and are dropped.
///
/// # Errors
///
/// The first error in `objects` order, from `fetch` or `consume`, or a
/// [`QueryError::Storage`] if no fetch thread could be started.
pub(crate) fn fetch_in_order<F, C>(
    objects: &[(String, u64)],
    limits: FetchLimits,
    fetch: F,
    mut consume: C,
) -> Result<(), QueryError>
where
    F: Fn(&str) -> Result<Vec<u8>, QueryError> + Sync,
    C: FnMut(usize, Vec<u8>) -> Result<(), QueryError>,
{
    if objects.is_empty() {
        return Ok(());
    }
    let sizes: Vec<u64> = objects.iter().map(|(_, size)| *size).collect();
    let shared = Shared {
        window: Mutex::new(Window::default()),
        changed: Condvar::new(),
    };
    let workers = limits.requests.clamp(1, objects.len());
    let span = tracing::Span::current();
    std::thread::scope(|scope| {
        let worker = || {
            span.in_scope(|| {
                while let Some(index) = shared.claim(&sizes, limits.bytes) {
                    let claim = Claim {
                        shared: &shared,
                        index,
                        done: false,
                    };
                    claim.complete(fetch(&objects[index].0));
                }
            });
        };
        let mut started = 0;
        for _ in 0..workers {
            match std::thread::Builder::new()
                .name("ourios-audit-fetch".to_owned())
                .spawn_scoped(scope, worker)
            {
                Ok(_) => started += 1,
                Err(e) if started == 0 => {
                    return Err(QueryError::Storage {
                        detail: format!("start audit fetch thread: {e}"),
                    });
                }
                // Fewer workers only lowers the concurrency; one is enough
                // to make progress.
                Err(_) => break,
            }
        }
        let outcome = consume_in_order(&shared, &sizes, &mut consume);
        shared.stop();
        outcome
    })
}

fn consume_in_order<C>(shared: &Shared, sizes: &[u64], consume: &mut C) -> Result<(), QueryError>
where
    C: FnMut(usize, Vec<u8>) -> Result<(), QueryError>,
{
    for (index, size) in sizes.iter().enumerate() {
        let outcome = shared.take(index).and_then(|body| consume(index, body));
        shared.release(*size);
        outcome?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    fn objects(sizes: &[u64]) -> Vec<(String, u64)> {
        sizes
            .iter()
            .enumerate()
            .map(|(i, size)| (format!("k{i:04}"), *size))
            .collect()
    }

    /// A body that names its key, so the consumer can check it got the
    /// right object at the right index.
    fn body(key: &str) -> Vec<u8> {
        key.as_bytes().to_vec()
    }

    /// Later objects return sooner, so completions arrive out of order.
    fn delay(key: &str) -> Duration {
        let index: u64 = key[1..].parse().expect("index");
        Duration::from_millis(20u64.saturating_sub(index % 20))
    }

    #[test]
    fn consumes_in_listing_order_despite_out_of_order_completion() {
        let objects = objects(&[10; 64]);
        let mut seen = Vec::new();

        fetch_in_order(
            &objects,
            FetchLimits {
                requests: 8,
                bytes: u64::MAX,
            },
            |key| {
                std::thread::sleep(delay(key));
                Ok(body(key))
            },
            |index, bytes| {
                assert_eq!(bytes, body(&objects[index].0));
                seen.push(index);
                Ok(())
            },
        )
        .expect("fetch");

        assert_eq!(seen, (0..objects.len()).collect::<Vec<_>>());
    }

    #[test]
    fn a_failed_get_fails_the_pass_in_order() {
        let objects = objects(&[10; 40]);
        let mut consumed = Vec::new();

        let err = fetch_in_order(
            &objects,
            FetchLimits::default(),
            |key| {
                std::thread::sleep(delay(key));
                match key {
                    "k0017" => Err(QueryError::Storage {
                        detail: format!("GET {key}"),
                    }),
                    _ => Ok(body(key)),
                }
            },
            |index, _| {
                consumed.push(index);
                Ok(())
            },
        )
        .expect_err("a failed GET fails the pass");

        assert!(matches!(err, QueryError::Storage { detail } if detail == "GET k0017"));
        assert_eq!(
            consumed,
            (0..17).collect::<Vec<_>>(),
            "everything before it folded"
        );
    }

    #[test]
    fn a_failed_consume_stops_the_pass() {
        let objects = objects(&[10; 200]);
        let fetched = AtomicUsize::new(0);

        let err = fetch_in_order(
            &objects,
            FetchLimits {
                requests: 4,
                bytes: 40,
            },
            |key| {
                fetched.fetch_add(1, Ordering::SeqCst);
                Ok(body(key))
            },
            |index, _| match index {
                3 => Err(QueryError::Storage {
                    detail: "decode".to_owned(),
                }),
                _ => Ok(()),
            },
        )
        .expect_err("a failed consume fails the pass");

        assert!(matches!(err, QueryError::Storage { .. }));
        assert!(
            fetched.load(Ordering::SeqCst) < objects.len(),
            "dispatch stops after the failure",
        );
    }

    #[test]
    fn a_panicking_get_fails_the_pass_instead_of_hanging() {
        let objects = objects(&[10; 8]);
        let outcome = std::panic::catch_unwind(|| {
            fetch_in_order(
                &objects,
                FetchLimits::default(),
                |key| {
                    assert_ne!(key, "k0005", "GET panics");
                    Ok(body(key))
                },
                |_, _| Ok(()),
            )
        });
        // The scope re-raises the worker's panic once the consumer returns;
        // reaching here at all is the no-hang guarantee.
        assert!(outcome.is_err());
    }

    /// Tracks GETs in flight and listed bytes outstanding (dispatched, not
    /// yet consumed), keeping their high-water marks.
    #[derive(Default)]
    struct Gauge {
        requests: AtomicUsize,
        max_requests: AtomicUsize,
        bytes: AtomicU64,
        max_bytes: AtomicU64,
    }

    fn bounded_pass(sizes: &[u64], limits: FetchLimits) -> Gauge {
        let objects = objects(sizes);
        let gauge = Gauge::default();
        fetch_in_order(
            &objects,
            limits,
            |key| {
                let index: usize = key[1..].parse().expect("index");
                let bytes = gauge.bytes.fetch_add(sizes[index], Ordering::SeqCst) + sizes[index];
                gauge.max_bytes.fetch_max(bytes, Ordering::SeqCst);
                let requests = gauge.requests.fetch_add(1, Ordering::SeqCst) + 1;
                gauge.max_requests.fetch_max(requests, Ordering::SeqCst);
                std::thread::sleep(delay(key));
                gauge.requests.fetch_sub(1, Ordering::SeqCst);
                Ok(body(key))
            },
            |index, _| {
                std::thread::sleep(Duration::from_millis(1));
                gauge.bytes.fetch_sub(sizes[index], Ordering::SeqCst);
                Ok(())
            },
        )
        .expect("fetch");
        gauge
    }

    #[test]
    fn in_flight_requests_and_bytes_stay_within_the_limits() {
        let sizes: Vec<u64> = (0..120).map(|i| 1_000 + (i * 37) % 900).collect();
        let limits = FetchLimits {
            requests: 6,
            bytes: 5_000,
        };

        let gauge = bounded_pass(&sizes, limits);

        let max_requests = gauge.max_requests.load(Ordering::SeqCst);
        let max_bytes = gauge.max_bytes.load(Ordering::SeqCst);
        assert!(
            max_requests <= limits.requests,
            "{max_requests} GETs in flight"
        );
        assert!(max_requests > 1, "the pass ran GETs concurrently");
        assert!(max_bytes <= limits.bytes, "{max_bytes} B outstanding");
    }

    #[test]
    fn an_object_larger_than_the_budget_is_fetched_alone() {
        let sizes = [100, 100, 10_000, 100, 100];
        let limits = FetchLimits {
            requests: 4,
            bytes: 1_000,
        };

        let gauge = bounded_pass(&sizes, limits);

        assert_eq!(gauge.max_bytes.load(Ordering::SeqCst), 10_000);
    }
}
