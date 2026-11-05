//! A server shutdown ends every `StreamAppend` call, whatever its peer
//! or its group is doing.

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::StreamExt as _;
use tonic::codegen::http::uri::PathAndQuery;
use tonic::{Code, Request};
use tonic_prost::ProstCodec;

use super::super::wire::{AppendRequest, StreamAppendResponse};
use super::super::{Peers, ReplicationServer, STREAM_APPEND};
use super::{Node, bind, consecutive, serve, wire_append};

/// `requests` sent as one `StreamAppend` stream, held open after its last
/// message, and the replies.
async fn open_raw<S>(
    addr: SocketAddr,
    requests: S,
) -> Result<tonic::Streaming<StreamAppendResponse>, Code>
where
    S: futures_util::Stream<Item = AppendRequest> + Send + 'static,
{
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    let mut grpc = tonic::client::Grpc::new(channel);
    grpc.ready().await.expect("ready");
    let codec = ProstCodec::<AppendRequest, StreamAppendResponse>::default();
    let request = Request::new(requests.chain(futures_util::stream::pending()));
    let path = PathAndQuery::from_static(STREAM_APPEND);
    grpc.streaming(request, path, codec)
        .await
        .map(tonic::Response::into_inner)
        .map_err(|s| s.code())
}

#[tokio::test]
async fn a_server_shutdown_ends_a_stream_still_waiting_for_its_first_message() {
    let (incoming, addr) = bind();
    let (stop, serving) = serve(ReplicationServer::default(), incoming);
    let opening = tokio::spawn(open_raw(addr, futures_util::stream::empty()));
    tokio::time::sleep(Duration::from_millis(200)).await;
    let _ = stop.send(());
    tokio::time::timeout(Duration::from_secs(10), serving)
        .await
        .expect("server stops")
        .expect("server task");
    let opened = tokio::time::timeout(Duration::from_secs(10), opening)
        .await
        .expect("call ends")
        .expect("call task");
    assert_eq!(opened.err(), Some(Code::Unavailable));
}

#[tokio::test]
async fn a_server_shutdown_ends_an_open_idle_stream_while_its_group_runs() {
    let (incoming, addr) = bind();
    let peers = Peers::plaintext();
    let mut node = Node::start(1, incoming, &peers, ReplicationServer::default()).await;
    let valid = wire_append(node.id, &consecutive(&[1])[0]);
    let mut replies = open_raw(addr, futures_util::stream::iter([valid]))
        .await
        .expect("open");
    assert!(replies.message().await.expect("answered").is_some());
    let _ = node.stop.take().expect("serving").send(());
    tokio::time::timeout(Duration::from_secs(10), &mut node.serving)
        .await
        .expect("server stops")
        .expect("server task");
    let ended = replies.message().await;
    assert_eq!(ended.err().map(|s| s.code()), Some(Code::Unavailable));
    node.raft.shutdown().await.expect("shutdown");
}
