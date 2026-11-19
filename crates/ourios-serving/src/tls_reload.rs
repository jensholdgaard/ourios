//! RFC 0030 §3.2 (RFC0030.6): the certificate-reload loop. It re-reads a
//! certificate, key and client CA on an interval and, on a content change,
//! swaps in what they configure: a listener's acceptor, or the client
//! configuration a node calls its peers with (RFC 0065 §6.8, #1175). An
//! unreadable or invalid reload logs, counts
//! `ourios.receiver.tls.reload_failures` and keeps the last good config.

use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use ourios_semconv as semconv;
use tokio_rustls::TlsAcceptor;

use crate::metrics::ERROR_TYPE;
use crate::tls::TlsSettings;

/// What a reload rebuilds from the certificate files.
pub(crate) trait Rebuild: Send + Sync + Sized + 'static {
    /// Build it from `settings`, advertising `alpn` where it has a say.
    fn rebuild(settings: &TlsSettings, alpn: &[Vec<u8>]) -> Result<Self, String>;
}

impl Rebuild for TlsAcceptor {
    fn rebuild(settings: &TlsSettings, alpn: &[Vec<u8>]) -> Result<Self, String> {
        let alpn: Vec<&[u8]> = alpn.iter().map(Vec::as_slice).collect();
        settings.acceptor(&alpn)
    }
}

impl Rebuild for Arc<rustls::ClientConfig> {
    fn rebuild(settings: &TlsSettings, _alpn: &[Vec<u8>]) -> Result<Self, String> {
        settings.client_config().map(Arc::new)
    }
}

/// When `settings.reload_interval` is set, spawn the task that keeps
/// `current` following `settings`' files, counting failures under
/// `listener`. The task stops once every handle to `current` is dropped.
pub(crate) fn spawn<T: Rebuild>(
    settings: &TlsSettings,
    alpn: Vec<Vec<u8>>,
    current: &Arc<RwLock<T>>,
    listener: &'static str,
) {
    let Some(interval) = settings.reload_interval else {
        return;
    };
    let task = ReloadTask {
        settings: settings.clone(),
        alpn,
        current: Arc::downgrade(current),
        failures: ourios_core::telemetry::meter("ourios.receiver")
            .u64_counter(semconv::OURIOS_RECEIVER_TLS_RELOAD_FAILURES)
            .with_description(semconv::OURIOS_RECEIVER_TLS_RELOAD_FAILURES_DESCRIPTION)
            .with_unit(semconv::OURIOS_RECEIVER_TLS_RELOAD_FAILURES_UNIT)
            .build(),
        listener,
    };
    // Seed with the startup material's fingerprint, taken now rather
    // than when the task first runs, so the first tick only reloads
    // if the files changed since the config was built — and a change
    // made before the task is scheduled is not missed.
    let initial = fingerprint(settings);
    tokio::spawn(task.run(interval, initial));
}

/// The client configuration a node calls its peers with, following its
/// certificate files (#1175). Clones share it; each new connection takes
/// the current config, and one already open keeps its session.
#[derive(Clone)]
pub struct ReloadingClientConfig {
    current: Arc<RwLock<Arc<rustls::ClientConfig>>>,
}

impl ReloadingClientConfig {
    /// `config`, never reloaded.
    #[must_use]
    pub fn fixed(config: rustls::ClientConfig) -> Self {
        Self {
            current: Arc::new(RwLock::new(Arc::new(config))),
        }
    }

    /// The config for a new connection.
    #[must_use]
    pub fn current(&self) -> Arc<rustls::ClientConfig> {
        Arc::clone(&self.current.read().unwrap_or_else(PoisonError::into_inner))
    }
}

/// A [`ReloadingClientConfig`] for `settings`, reloaded on
/// `settings.reload_interval` and counted under `listener`, the listener
/// that serves the same certificate.
///
/// # Errors
///
/// Whatever [`TlsSettings::client_config`] reports for the initial build.
pub fn reloading_client_config(
    settings: &TlsSettings,
    listener: &'static str,
) -> Result<ReloadingClientConfig, String> {
    let current = Arc::new(RwLock::new(Arc::new(settings.client_config()?)));
    spawn(settings, Vec::new(), &current, listener);
    Ok(ReloadingClientConfig { current })
}

/// `ourios.tls.reload_error` values.
const RELOAD_UNREADABLE: &str = "unreadable";
const RELOAD_INVALID: &str = "invalid";
/// The reload task itself unwound — the `error.type` of the reload event
/// only; the counter records material failures.
const RELOAD_PANIC: &str = "panic";

/// The outcome of one reload attempt (computed off the async worker in
/// `spawn_blocking`, since it reads + parses PEM files). `Reloaded`
/// carries the rebuilt config and the new fingerprint; the failure
/// variants keep the last good config.
enum ReloadOutcome<T> {
    Unchanged,
    Reloaded(u64, Box<T>),
    Unreadable,
    Invalid(String),
}

/// A stable fingerprint of the cert + key (+ CA) file *contents* — a
/// hash, not the bytes, so the reload task never retains a long-lived
/// copy of the private key just to detect changes. `None` when any file
/// is unreadable. Change detection only; a hash collision (missing a
/// rotation) is astronomically unlikely.
fn fingerprint(settings: &TlsSettings) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::fs::read(&settings.cert_file).ok()?.hash(&mut hasher);
    std::fs::read(&settings.key_file).ok()?.hash(&mut hasher);
    if let Some(ca) = &settings.client_ca_file {
        std::fs::read(ca).ok()?.hash(&mut hasher);
    }
    Some(hasher.finish())
}

/// Read the files, and if they changed, rebuild what they configure. All
/// the blocking filesystem + PEM work, so the caller runs it in
/// `spawn_blocking`.
fn reload_once<T: Rebuild>(
    settings: &TlsSettings,
    alpn: &[Vec<u8>],
    last: Option<u64>,
) -> ReloadOutcome<T> {
    #[cfg(feature = "fault-injection")]
    assert!(
        !PANIC_NEXT_RELOAD.swap(false, std::sync::atomic::Ordering::AcqRel),
        "injected TLS reload panic"
    );
    let Some(fp) = fingerprint(settings) else {
        return ReloadOutcome::Unreadable;
    };
    if Some(fp) == last {
        return ReloadOutcome::Unchanged;
    }
    match T::rebuild(settings, alpn) {
        Ok(rebuilt) => ReloadOutcome::Reloaded(fp, Box::new(rebuilt)),
        Err(e) => ReloadOutcome::Invalid(e),
    }
}

#[cfg(feature = "fault-injection")]
static PANIC_NEXT_RELOAD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Make the next certificate reload, on any listener, unwind inside its
/// blocking task — the one reload failure no file on disk can cause.
///
/// Behind the `fault-injection` feature, which only this crate's own
/// test targets enable. Not part of the stable public API.
#[cfg(feature = "fault-injection")]
pub fn panic_next_reload() {
    PANIC_NEXT_RELOAD.store(true, std::sync::atomic::Ordering::Release);
}

/// What one reload attempt came to.
enum Attempt<T> {
    Done(ReloadOutcome<T>),
    /// It panicked: logged, and tried again at the next tick.
    Retry,
    /// The runtime is shutting down.
    Stop,
}

/// The certificate-reload loop behind a reloading acceptor or client
/// config.
struct ReloadTask<T> {
    settings: TlsSettings,
    alpn: Vec<Vec<u8>>,
    current: std::sync::Weak<RwLock<T>>,
    failures: Counter<u64>,
    listener: &'static str,
}

impl<T: Rebuild> ReloadTask<T> {
    async fn run(self, interval: Duration, initial: Option<u64>) {
        let mut last = initial;
        let mut tick = tokio::time::interval(interval);
        // Steady cadence: a long reload or a paused runtime must not
        // make the interval "catch up" with a burst of back-to-back
        // re-reads (mirrors the age-sweep task).
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await; // the first tick is immediate; skip it
        loop {
            tick.tick().await;
            // Stop once the listener drops its acceptor.
            if self.current.strong_count() == 0 {
                break;
            }
            let outcome = match self.attempt(last).await {
                Attempt::Done(outcome) => outcome,
                Attempt::Retry => continue,
                Attempt::Stop => break,
            };
            // Re-check the acceptor is still alive after the await.
            let Some(current) = self.current.upgrade() else {
                break;
            };
            self.apply(outcome, &current, &mut last);
        }
    }

    /// One reload attempt, off the async worker: the read, parse and
    /// rebuild block.
    async fn attempt(&self, last: Option<u64>) -> Attempt<T> {
        let (s, a) = (self.settings.clone(), self.alpn.clone());
        match tokio::task::spawn_blocking(move || reload_once::<T>(&s, &a, last)).await {
            Ok(outcome) => Attempt::Done(outcome),
            // Cancelled → the runtime is shutting down; stop.
            Err(e) if e.is_cancelled() => Attempt::Stop,
            // A panic in the (pure) reload path is a bug, not a shutdown —
            // log and retry next tick rather than silently disabling all
            // future rotations.
            Err(e) => {
                tracing::error!(
                    name: semconv::EVENT_OURIOS_SERVER_TLS_RELOAD_ERROR,
                    {
                        { ERROR_TYPE } = RELOAD_PANIC,
                        { semconv::OURIOS_SERVER_LISTENER_NAME } = self.listener,
                    },
                    "TLS reload task panicked (cert {}, key {}, client CA {:?}); retrying next \
                     tick: {e}",
                    self.settings.cert_file.display(),
                    self.settings.key_file.display(),
                    self.settings.client_ca_file,
                );
                Attempt::Retry
            }
        }
    }

    fn apply(&self, outcome: ReloadOutcome<T>, current: &RwLock<T>, last: &mut Option<u64>) {
        let settings = &self.settings;
        match outcome {
            ReloadOutcome::Unchanged => {}
            ReloadOutcome::Reloaded(fp, rebuilt) => {
                *current.write().unwrap_or_else(PoisonError::into_inner) = *rebuilt;
                *last = Some(fp);
                tracing::info!(
                    name: semconv::EVENT_OURIOS_SERVER_TLS_RELOAD_COMPLETED,
                    { { semconv::OURIOS_SERVER_LISTENER_NAME } = self.listener },
                    "reloaded the TLS certificate from {}",
                    settings.cert_file.display(),
                );
            }
            ReloadOutcome::Unreadable => {
                self.count_failure(RELOAD_UNREADABLE);
                tracing::warn!(
                    name: semconv::EVENT_OURIOS_SERVER_TLS_RELOAD_ERROR,
                    {
                        { ERROR_TYPE } = RELOAD_UNREADABLE,
                        { semconv::OURIOS_SERVER_LISTENER_NAME } = self.listener,
                    },
                    "TLS material unreadable on reload (cert {}, key {}, client CA {:?}); \
                     keeping the last good certificate",
                    settings.cert_file.display(),
                    settings.key_file.display(),
                    settings.client_ca_file,
                );
            }
            ReloadOutcome::Invalid(e) => {
                self.count_failure(RELOAD_INVALID);
                tracing::error!(
                    name: semconv::EVENT_OURIOS_SERVER_TLS_RELOAD_ERROR,
                    {
                        { ERROR_TYPE } = RELOAD_INVALID,
                        { semconv::OURIOS_SERVER_LISTENER_NAME } = self.listener,
                    },
                    "TLS reload produced an invalid config (cert {}, key {}, client CA {:?}); \
                     keeping the last good certificate: {e}",
                    settings.cert_file.display(),
                    settings.key_file.display(),
                    settings.client_ca_file,
                );
            }
        }
    }

    fn count_failure(&self, class: &'static str) {
        self.failures.add(
            1,
            &[
                KeyValue::new(semconv::OURIOS_SERVER_LISTENER_NAME, self.listener),
                KeyValue::new(semconv::OURIOS_TLS_RELOAD_ERROR, class),
            ],
        );
    }
}
