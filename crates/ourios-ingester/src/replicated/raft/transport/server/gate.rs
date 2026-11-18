//! The server's gate (RFC0065.13): what it makes of each call from who
//! the caller's certificate names, before anything of it is decoded.

use std::convert::Infallible;
use std::sync::Arc;

use tonic::Status;
use tonic::body::Body;
use tonic::codegen::{BoxFuture, http};

use super::super::tail::{self, Clients, TailTarget};
use super::super::trace::{Call, Tracing};
use super::super::{
    APPEND, FORWARD, IDENTIFY, INSTALL_SNAPSHOT, PRE_VOTE, PROPOSE, READ_INDEX, STREAM_APPEND,
    TAIL, TAIL_SCAN, VOTE,
};

/// A node's tail target, and who may call it.
pub(super) struct Tails {
    target: Option<Arc<dyn TailTarget>>,
    clients: Clients,
}

impl Default for Tails {
    /// No tail, and no caller restricted.
    fn default() -> Self {
        Self {
            target: None,
            clients: Clients::only(Vec::<String>::new()),
        }
    }
}

impl Tails {
    pub(super) fn new(target: Arc<dyn TailTarget>, clients: Clients) -> Self {
        Self {
            target: Some(target),
            clients,
        }
    }

    pub(super) fn admit(&mut self, clients: Clients) {
        self.clients = clients;
    }

    pub(super) fn serve(&mut self, target: Arc<dyn TailTarget>) {
        self.target = Some(target);
    }

    /// What the gate makes of a call, before anything of it is decoded
    /// (RFC0065.13): a tail call from a caller among the tail's clients
    /// goes to the target that admitted it, a tail call from any other is
    /// denied, and any other call from a listed caller is refused, since a
    /// querier's certificate reads the tail and nothing else.
    pub(super) fn gate<B>(&self, request: &http::Request<B>) -> Gated {
        let Some(method) = method_of(request.uri().path()) else {
            return Gated::Pass;
        };
        let identities = tail::client_identities(request);
        match method {
            TAIL | TAIL_SCAN if self.clients.admits(&identities) => {
                Gated::Tail(self.target.clone())
            }
            TAIL | TAIL_SCAN => Gated::Refused(method, tail::denied()),
            _ if self.clients.lists(&identities) => Gated::Refused(
                method,
                Status::permission_denied("a tail client may only read the tail"),
            ),
            _ => Gated::Pass,
        }
    }
}

/// What the server's gate makes of a call.
pub(super) enum Gated {
    /// Dispatch it.
    Pass,
    /// Dispatch a tail call to the target its caller was admitted to.
    Tail(Option<Arc<dyn TailTarget>>),
    /// Answer the method's call with this refusal, undecoded.
    Refused(&'static str, Status),
}

/// The method `path` names, if it is one this server serves.
fn method_of(path: &str) -> Option<&'static str> {
    [
        APPEND,
        VOTE,
        PRE_VOTE,
        INSTALL_SNAPSHOT,
        STREAM_APPEND,
        FORWARD,
        PROPOSE,
        READ_INDEX,
        IDENTIFY,
        TAIL,
        TAIL_SCAN,
    ]
    .into_iter()
    .find(|method| *method == path)
}

/// Answer `request` with `status` without decoding it, under the server
/// span its caller's propagated context parents (RFC 0065 §6.9).
pub(super) fn refuse_undecoded<B>(
    tracing: Tracing,
    method: &'static str,
    request: &http::Request<B>,
    status: Status,
) -> BoxFuture<http::Response<Body>, Infallible> {
    let headers = tonic::metadata::MetadataMap::from_headers(request.headers().clone());
    let caller = ourios_serving::extract_context_from_metadata(&headers);
    Call::server(tracing, method, true, caller).finish(status.code());
    Box::pin(async move { Ok(status.into_http()) })
}
