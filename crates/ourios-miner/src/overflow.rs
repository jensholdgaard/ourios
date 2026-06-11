//! RFC 0001 §6.5 — per-parameter byte limit + OVERFLOW marker.
//!
//! When a masked-parameter value's byte length exceeds the
//! configured limit (default 256 B, ceiling 1 KiB per
//! `CLAUDE.md` §3.2), the [`Param`] slot is replaced with an
//! `Overflow` marker carrying `(length: u32, sha256_prefix: [u8;
//! 8])`. The marker preserves enough information for
//! equality-shaped queries ("find rows where this exact long
//! param occurred") without storing the long bytes in the
//! columnar data; reconstruction falls back to the retained
//! `body` for any record with an `Overflow` param (RFC §6.6).
//!
//! The §3.2 invariant the marker defends: unbounded `params`
//! cardinality destroys Parquet's dictionary encoding and bloats
//! files. The byte-limit + spill-to-body contract is the answer
//! to the canonical "stack trace in a `params` slot" hazard
//! (`docs/hazards.md` H2).

use sha2::{Digest, Sha256};

use ourios_core::audit::ParamType;
use ourios_core::record::Param;

/// Byte length of the SHA-256 prefix carried in an `Overflow`
/// marker. RFC 0001 §6.5 pins this at 8 bytes — long enough that
/// a collision over a per-tenant param population is
/// astronomically unlikely while keeping the on-disk marker
/// compact.
pub const OVERFLOW_PREFIX_BYTES: usize = 8;

/// Apply the §6.5 byte-limit check to a single parameter.
///
/// `byte_limit` is the configured per-parameter ceiling (the
/// `MinerConfig::param_byte_limit` for the tenant). If
/// `value.len() <= byte_limit`, the function returns
/// `Param { type_tag, value }` unchanged. If `value.len() >
/// byte_limit`, the function returns an `Overflow` marker whose
/// `value` encodes the original length and the SHA-256 prefix of
/// the original bytes (RFC §6.5).
///
/// The marker's textual encoding is
/// `"OVERFLOW(length={N},sha256={HEX})"` where `HEX` is the
/// lowercase hex of the first [`OVERFLOW_PREFIX_BYTES`] of the
/// SHA-256 digest (16 hex chars). The format is deterministic
/// (callers can parse `length` for size analytics, or join on
/// `sha256` to find equal long values across rows). The original
/// `type_tag` is discarded — Overflow supersedes it; the original
/// bytes spill to the record's `body` column unconditionally
/// (the caller is responsible for setting `body = Some(raw)` when
/// any param overflows, per §6.6's reconstruction-via-body
/// fallback).
#[must_use]
pub fn cap_param_value(type_tag: ParamType, value: String, byte_limit: u32) -> Param {
    if value.len() as u64 <= u64::from(byte_limit) {
        return Param { type_tag, value };
    }
    let length: u32 = u32::try_from(value.len()).unwrap_or(u32::MAX);
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(OVERFLOW_PREFIX_BYTES * 2);
    for byte in &digest[..OVERFLOW_PREFIX_BYTES] {
        // Lowercase hex per RFC convention; `{:02x}` is the
        // standard formatter. Two chars per byte ⇒ 16 chars total
        // for the 8-byte prefix.
        use std::fmt::Write;
        write!(&mut hex, "{byte:02x}").expect("writing to String never fails");
    }
    Param {
        type_tag: ParamType::Overflow,
        value: format!("OVERFLOW(length={length},sha256={hex})"),
    }
}

/// `true` iff any [`Param`] in the record's params vector is an
/// `Overflow` marker. The caller uses this to force body
/// retention on the emitted record per §6.5 / §6.6.
#[must_use]
pub fn any_overflow(params: &[Param]) -> bool {
    params.iter().any(|p| p.type_tag == ParamType::Overflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_param_value_passes_through_when_under_limit() {
        let p = cap_param_value(ParamType::Num, "42".to_string(), 256);
        assert_eq!(p.type_tag, ParamType::Num);
        assert_eq!(p.value, "42");
    }

    #[test]
    fn cap_param_value_passes_through_at_exact_limit() {
        // Boundary: value.len() == byte_limit is the largest
        // accepted value (inclusive).
        let value = "x".repeat(256);
        let p = cap_param_value(ParamType::Str, value.clone(), 256);
        assert_eq!(p.type_tag, ParamType::Str);
        assert_eq!(p.value, value);
    }

    #[test]
    fn cap_param_value_emits_overflow_marker_at_limit_plus_one() {
        // Boundary: value.len() == byte_limit + 1 trips the
        // overflow branch.
        let value = "x".repeat(257);
        let p = cap_param_value(ParamType::Str, value, 256);
        assert_eq!(p.type_tag, ParamType::Overflow);
        assert!(
            p.value.starts_with("OVERFLOW(length=257,sha256="),
            "unexpected marker: {}",
            p.value,
        );
        assert!(p.value.ends_with(')'), "marker must close: {}", p.value);
        // Hex prefix is exactly 16 chars (8 bytes × 2).
        let hex_start = p.value.find("sha256=").unwrap() + "sha256=".len();
        let hex_end = p.value.len() - 1; // strip the trailing ')'
        assert_eq!(
            hex_end - hex_start,
            OVERFLOW_PREFIX_BYTES * 2,
            "sha256 hex must be {} chars",
            OVERFLOW_PREFIX_BYTES * 2,
        );
        assert!(
            p.value[hex_start..hex_end]
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "sha256 must be lowercase ASCII hex: {}",
            &p.value[hex_start..hex_end],
        );
    }

    #[test]
    fn cap_param_value_marker_is_deterministic_for_same_input() {
        // The §6.5 "find rows where this exact long param
        // occurred" use case depends on the marker being stable
        // for byte-identical inputs. Two calls on the same value
        // must produce equal markers.
        let value = "stack trace…".repeat(40); // ~480 bytes
        let p1 = cap_param_value(ParamType::Str, value.clone(), 256);
        let p2 = cap_param_value(ParamType::Str, value, 256);
        assert_eq!(p1.value, p2.value);
    }

    #[test]
    fn cap_param_value_distinct_inputs_produce_distinct_markers() {
        // Different values past the threshold should hash to
        // different prefixes (collision space is 2^64 — never on
        // a unit-test corpus). Sanity check.
        let v1 = "a".repeat(300);
        let v2 = "b".repeat(300);
        let p1 = cap_param_value(ParamType::Str, v1, 256);
        let p2 = cap_param_value(ParamType::Str, v2, 256);
        assert_ne!(p1.value, p2.value);
    }

    #[test]
    fn any_overflow_detects_overflow_in_mixed_params() {
        let params = vec![
            Param {
                type_tag: ParamType::Num,
                value: "42".to_string(),
            },
            Param {
                type_tag: ParamType::Overflow,
                value: "OVERFLOW(length=512,sha256=0123456789abcdef)".to_string(),
            },
        ];
        assert!(any_overflow(&params));
    }

    #[test]
    fn any_overflow_is_false_when_no_overflow_present() {
        let params = vec![Param {
            type_tag: ParamType::Num,
            value: "42".to_string(),
        }];
        assert!(!any_overflow(&params));
    }
}
