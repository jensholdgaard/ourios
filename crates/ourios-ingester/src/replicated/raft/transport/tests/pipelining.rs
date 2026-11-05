//! A stream carries appends without waiting for their answers.

use std::convert::Infallible;
use std::task::Poll;
use std::time::Duration;

use futures_util::StreamExt as _;
use openraft::network::{RPCOption, v2::RaftNetworkV2};
use tokio::sync::watch;
use tonic::codegen::{Body as HttpBody, BoxFuture, StdError, http};
use tonic::{Request, Status};
use tonic_prost::ProstCodec;

use super::super::wire::{AppendRequest, StreamAppendResponse};
use super::super::{Peers, SERVICE};
use super::{bind, client_to, consecutive};
use crate::replicated::raft::types::NodeId;

/// A peer that opens every `StreamAppend` stream, counts the messages it
/// receives, and answers none of them.
#[derive(Clone)]
struct Counting {
    received: watch::Sender<usize>,
}

impl tonic::server::StreamingService<AppendRequest> for Counting {
    type Response = StreamAppendResponse;
    type ResponseStream =
        futures_util::stream::BoxStream<'static, Result<StreamAppendResponse, Status>>;
    type Future = BoxFuture<tonic::Response<Self::ResponseStream>, Status>;

    fn call(&mut self, request: Request<tonic::Streaming<AppendRequest>>) -> Self::Future {
        let received = self.received.clone();
        let read = futures_util::stream::unfold(request.into_inner(), move |mut incoming| {
            let received = received.clone();
            async move {
                while let Ok(Some(_)) = incoming.message().await {
                    received.send_modify(|n| *n += 1);
                }
                std::future::pending::<Option<(Result<StreamAppendResponse, Status>, _)>>().await
            }
        });
        Box::pin(async move { Ok(tonic::Response::new(read.boxed())) })
    }
}

impl<B> tonic::codegen::Service<http::Request<B>> for Counting
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
        let service = self.clone();
        Box::pin(async move {
            let codec = ProstCodec::<StreamAppendResponse, AppendRequest>::default();
            Ok(tonic::server::Grpc::new(codec)
                .streaming(service, request)
                .await)
        })
    }
}

impl tonic::server::NamedService for Counting {
    const NAME: &'static str = SERVICE;
}

/// Every append reaches the peer before any answer comes back: a
/// sequential transport would send the second only after the first's.
#[tokio::test]
async fn a_stream_sends_appends_before_their_answers_arrive() {
    let (incoming, addr) = bind();
    let (received, mut counted) = watch::channel(0);
    let serving = tokio::spawn(
        tonic::transport::Server::builder().serve_with_incoming(Counting { received }, incoming),
    );
    let mut client = client_to(&Peers::plaintext(), NodeId::from(1), addr).await;
    let option = RPCOption::new(Duration::from_secs(5));
    let input =
        futures_util::stream::iter(consecutive(&[1, 1, 1])).chain(futures_util::stream::pending());
    let answers = client.stream_append(input, option).await.expect("open");
    tokio::time::timeout(Duration::from_secs(5), counted.wait_for(|n| *n == 3))
        .await
        .expect("all three appends in flight at once")
        .expect("peer serving");
    drop(answers);
    serving.abort();
}
