//! The injection half of a call between Ourios's nodes (RFC 0065 §6.9).

use opentelemetry::Context;
use opentelemetry::propagation::TextMapPropagator as _;
use opentelemetry::trace::{
    SpanContext, SpanId, TraceContextExt as _, TraceFlags, TraceId, TraceState,
};
use opentelemetry_sdk::propagation::TraceContextPropagator;

use super::{MetadataExtractor, MetadataInjector};

/// A context written into gRPC metadata by `MetadataInjector` reads back
/// through `MetadataExtractor` as the same span, now remote.
#[test]
fn a_context_injected_into_grpc_metadata_extracts_as_the_same_span() {
    let local = SpanContext::new(
        TraceId::from_hex("4bf92f3577b34da6a3ce929d0e0e4736").expect("trace id"),
        SpanId::from_hex("00f067aa0ba902b7").expect("span id"),
        TraceFlags::SAMPLED,
        false,
        TraceState::default(),
    );
    let propagator = TraceContextPropagator::new();
    let mut metadata = tonic::metadata::MetadataMap::new();
    propagator.inject_context(
        &Context::new().with_remote_span_context(local.clone()),
        &mut MetadataInjector(&mut metadata),
    );

    let read = propagator.extract(&MetadataExtractor(&metadata));
    let span = read.span();
    let sc = span.span_context();
    assert_eq!(sc.trace_id(), local.trace_id());
    assert_eq!(sc.span_id(), local.span_id());
    assert!(sc.is_sampled());
    assert!(sc.is_remote());
}

/// A context without a span writes nothing, so the callee roots its own.
#[test]
fn a_context_without_a_span_injects_nothing() {
    let mut metadata = tonic::metadata::MetadataMap::new();
    TraceContextPropagator::new()
        .inject_context(&Context::new(), &mut MetadataInjector(&mut metadata));
    assert!(metadata.is_empty(), "{metadata:?}");
}
