//! RFC 0068 §3.3: a torn tail recovery cut off the newest WAL segment is
//! reported, never silent.

use opentelemetry::KeyValue;
use opentelemetry::metrics::Meter;
use ourios_wal::{TailHeal, TailShape};

/// Standard OpenTelemetry file attributes, deliberately not in the
/// Ourios registry.
const FILE_PATH: &str = "file.path";
const FILE_SIZE: &str = "file.size";

fn shape_value(shape: TailShape) -> &'static str {
    match shape {
        TailShape::PartialFrame => "partial_frame",
        TailShape::ZeroTail => "zero_tail",
    }
}

/// Log the heal as one `WARN` event and count the bytes it cut.
pub(crate) fn report(heal: &TailHeal, meter: &Meter) {
    let shape = shape_value(heal.shape);
    let cut = heal.size.saturating_sub(heal.offset);
    tracing::warn!(
        name: ourios_semconv::EVENT_OURIOS_WAL_RECOVERY_TAIL_TRUNCATED,
        {
            { FILE_PATH } = %heal.segment.display(),
            { FILE_SIZE } = heal.size,
            { ourios_semconv::OURIOS_WAL_RECOVERY_TAIL_OFFSET } = heal.offset,
            { ourios_semconv::OURIOS_WAL_RECOVERY_TAIL_SHAPE } = shape,
        },
        "WAL recovery cut a torn tail of {cut} bytes ({shape}) off the newest segment {}; \
         it held no acknowledged frame",
        heal.segment.display(),
    );
    meter
        .u64_counter(ourios_semconv::OURIOS_WAL_RECOVERY_TRUNCATED_SIZE)
        .with_description(ourios_semconv::OURIOS_WAL_RECOVERY_TRUNCATED_SIZE_DESCRIPTION)
        .with_unit(ourios_semconv::OURIOS_WAL_RECOVERY_TRUNCATED_SIZE_UNIT)
        .build()
        .add(
            cut,
            &[KeyValue::new(
                ourios_semconv::OURIOS_WAL_RECOVERY_TAIL_SHAPE,
                shape,
            )],
        );
}

#[cfg(test)]
mod tests;
