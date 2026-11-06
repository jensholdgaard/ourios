//! A peer that withholds its answers.

use std::convert::Infallible;
use std::task::Poll;
use std::time::Duration;

use futures_util::StreamExt as _;
use openraft::errors::RPCError;
use openraft::network::{RPCOption, v2::RaftNetworkV2 as _};
use tonic::codegen::{Body as HttpBody, BoxFuture, StdError, http};
use tonic::{Request, Status};
use tonic_prost::ProstCodec;

use super::super::super::types::NodeId;
use super::super::wire::{AppendRequest, StreamAppendResponse};
use super::super::{Peers, SERVICE};
use super::{bind, client_to, consecutive};

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
