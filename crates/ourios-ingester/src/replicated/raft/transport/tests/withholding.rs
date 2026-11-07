//! Peers that withhold their answers.

use std::convert::Infallible;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use futures_util::StreamExt as _;
use openraft::errors::RPCError;
use openraft::network::{RPCOption, v2::RaftNetworkV2 as _};
use tokio::sync::Notify;
use tonic::codegen::{Body as HttpBody, BoxFuture, StdError, http};
use tonic::{Request, Status};
use tonic_prost::ProstCodec;

use super::super::super::types::{NodeId, Vote};
use super::super::wire::{
    AppendRequest, StreamAppendResponse, WireVote, WireVoteRequest, WireVoteResponse,
};
use super::super::{Peers, SERVICE};
use super::{bind, client_to, consecutive, option, vote_request};

/// A peer that opens every `StreamAppend` stream and reads its messages
/// but answers none of them.
#[derive(Clone)]
struct Withholding;

impl tonic::server::StreamingService<AppendRequest> for Withholding {
    type Response = StreamAppendResponse;
    type ResponseStream =
        futures_util::stream::BoxStream<'static, Result<StreamAppendResponse, Status>>;
    type Future = BoxFuture<tonic::Response<Self::ResponseStream>, Status>;

    fn call(&mut self, request: Request<tonic::Streaming<AppendRequest>>) -> Self::Future {
        let read = futures_util::stream::unfold(request.into_inner(), |mut incoming| async move {
            while let Ok(Some(_)) = incoming.message().await {}
            std::future::pending::<Option<(Result<StreamAppendResponse, Status>, _)>>().await
        });
        Box::pin(async move { Ok(tonic::Response::new(read.boxed())) })
    }
}

impl<B> tonic::codegen::Service<http::Request<B>> for Withholding
where
    B: HttpBody + Send + 'static,
    B::Error: Into<StdError> + Send + 'static,
{
    type Response = http::Response<tonic::body::Body>;
    type Error = Infallible;
    type Future = BoxFuture<Self::Response, Self::Error>;

    fn poll_ready(&mut self, _cx: &mut std::task::Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        Box::pin(async move {
            let codec = ProstCodec::<StreamAppendResponse, AppendRequest>::default();
            Ok(tonic::server::Grpc::new(codec)
                .streaming(Withholding, request)
                .await)
        })
    }
}

impl tonic::server::NamedService for Withholding {
    const NAME: &'static str = SERVICE;
}

/// An open stream whose peer holds back an answer times out the wait for
/// it, rather than waiting as long as an idle stream may.
#[tokio::test]
async fn a_stream_whose_peer_withholds_an_answer_times_out() {
    let (incoming, addr) = bind();
    let serving = tokio::spawn(
        tonic::transport::Server::builder().serve_with_incoming(Withholding, incoming),
    );
    let mut client = client_to(&Peers::plaintext(), NodeId::from(1), addr).await;
    let option = RPCOption::new(Duration::from_millis(200));
    let input =
        futures_util::stream::iter(consecutive(&[1])).chain(futures_util::stream::pending());
    let mut answers = client.stream_append(input, option).await.expect("open");
    let answer = tokio::time::timeout(Duration::from_secs(5), answers.next())
        .await
        .expect("the wait for the answer ends");
    assert!(
        matches!(answer, Some(Err(RPCError::Timeout(_)))),
        "{answer:?}"
    );
    drop(answers);
    serving.abort();
}

/// A peer that answers each vote, granted, only once released.
#[derive(Clone, Default)]
struct Held {
    received: Arc<Notify>,
    release: Arc<Notify>,
}

impl tonic::server::UnaryService<WireVoteRequest> for Held {
    type Response = WireVoteResponse;
    type Future = BoxFuture<tonic::Response<WireVoteResponse>, Status>;

    fn call(&mut self, _: Request<WireVoteRequest>) -> Self::Future {
        let held = self.clone();
        Box::pin(async move {
            held.received.notify_one();
            held.release.notified().await;
            Ok(tonic::Response::new(WireVoteResponse {
                vote: Some(WireVote::from(&Vote::new(1, NodeId::from(1)))),
                vote_granted: true,
                last_log_id: None,
            }))
        })
    }
}

impl<B> tonic::codegen::Service<http::Request<B>> for Held
where
    B: HttpBody + Send + 'static,
    B::Error: Into<StdError> + Send + 'static,
{
    type Response = http::Response<tonic::body::Body>;
    type Error = Infallible;
    type Future = BoxFuture<Self::Response, Self::Error>;

    fn poll_ready(&mut self, _cx: &mut std::task::Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        let held = self.clone();
        Box::pin(async move {
            let codec = ProstCodec::<WireVoteResponse, WireVoteRequest>::default();
            Ok(tonic::server::Grpc::new(codec).unary(held, request).await)
        })
    }
}

impl tonic::server::NamedService for Held {
    const NAME: &'static str = SERVICE;
}

/// A call in flight when its link is cut fails as an unreachable peer,
/// even though the peer then answers it.
#[tokio::test]
async fn a_unary_answer_that_arrives_after_the_link_is_cut_is_unreachable() {
    let (incoming, addr) = bind();
    let held = Held::default();
    let serving = tokio::spawn(
        tonic::transport::Server::builder().serve_with_incoming(held.clone(), incoming),
    );
    let peers = Peers::plaintext();
    let mut client = client_to(&peers, NodeId::from(1), addr).await;
    let voting = tokio::spawn(async move { client.vote(vote_request(), option()).await });
    held.received.notified().await;
    peers.cut(NodeId::from(2), NodeId::from(1));
    held.release.notify_one();
    let answer = voting.await.expect("the vote task");
    assert!(
        matches!(answer, Err(RPCError::Unreachable(_))),
        "{answer:?}"
    );
    serving.abort();
}
