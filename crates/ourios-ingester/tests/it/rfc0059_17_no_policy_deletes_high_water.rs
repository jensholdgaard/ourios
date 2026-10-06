//! RFC0059.17 — No documented IAM policy can delete the high-water.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.

use ourios_ingester::template_ids::HIGH_WATER_KEY;
use serde_json::Value;

const CHART_README: &str = include_str!("../../../../deploy/helm/ourios/README.md");
const PREFIX: &str = "<prefix>/";
const DOCUMENTED_POLICIES: usize = 4;

/// The README's fenced `json` blocks that are IAM policies.
fn policies(markdown: &str) -> Vec<String> {
    markdown
        .split("```json\n")
        .skip(1)
        .filter_map(|block| block.split("```").next())
        .filter(|block| block.contains("\"Statement\""))
        .map(str::to_owned)
        .collect()
}

/// IAM wildcard matching: `*` any run, `?` one character.
fn glob(pattern: &[u8], text: &[u8]) -> bool {
    match (pattern.split_first(), text.split_first()) {
        (None, _) => text.is_empty(),
        (Some((b'*', rest)), _) => {
            glob(rest, text) || text.split_first().is_some_and(|(_, t)| glob(pattern, t))
        }
        (Some((p, rest)), Some((t, tail))) if *p == b'?' || p == t => glob(rest, tail),
        _ => false,
    }
}

fn values(statement: &Value, field: &str) -> Vec<String> {
    assert!(
        statement.get(format!("Not{field}")).is_none(),
        "Not{field} is not checked by this test: {statement}"
    );
    match &statement[field] {
        Value::String(one) => vec![one.clone()],
        Value::Array(many) => many
            .iter()
            .map(|v| v.as_str().expect("a string entry").to_owned())
            .collect(),
        other => panic!("{field} must be a string or a list, got {other}"),
    }
}

/// Whether `statement` applies `s3:DeleteObject` to `object`.
fn deletes(statement: &Value, object: &str) -> bool {
    let delete = values(statement, "Action")
        .iter()
        .any(|a| glob(a.to_ascii_lowercase().as_bytes(), b"s3:deleteobject"));
    delete
        && values(statement, "Resource")
            .iter()
            .any(|r| glob(r.as_bytes(), object.as_bytes()))
}

fn assert_high_water_undeletable(policy: &str, object: &str) {
    let policy: Value = serde_json::from_str(policy).expect("a documented policy parses");
    let statements = policy["Statement"].as_array().expect("a Statement list");
    for statement in statements {
        assert!(
            statement["Effect"] != "Allow" || !deletes(statement, object),
            "an allowed delete reaches {object}: {statement}"
        );
    }
    assert!(
        statements
            .iter()
            .any(|s| s["Effect"] == "Deny" && deletes(s, object)),
        "no explicit Deny protects {object} in {policy}"
    );
}

/// Scenario RFC0059.17 — every JSON policy in the chart README keeps
/// `s3:DeleteObject` off the high-water and denies it explicitly, with
/// and without a `storage.s3.prefix`.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[test]
fn rfc0059_17_no_documented_policy_can_delete_the_high_water() {
    let documented = policies(CHART_README);
    assert_eq!(
        documented.len(),
        DOCUMENTED_POLICIES,
        "the chart README's policy count changed; check the new one is covered"
    );
    for policy in &documented {
        let object = format!("arn:aws:s3:::<bucket>/{PREFIX}{HIGH_WATER_KEY}");
        assert_high_water_undeletable(policy, &object);
        let unprefixed = format!("arn:aws:s3:::<bucket>/{HIGH_WATER_KEY}");
        assert_high_water_undeletable(&policy.replace(PREFIX, ""), &unprefixed);
    }
}

#[test]
fn a_bucket_wide_delete_is_caught() {
    let policy = r#"{"Statement": [
        {"Effect": "Allow", "Action": "s3:*", "Resource": "arn:aws:s3:::<bucket>/*"},
        {"Effect": "Deny", "Action": "s3:DeleteObject", "Resource": "arn:aws:s3:::<bucket>/miner/*"}
    ]}"#;
    let object = format!("arn:aws:s3:::<bucket>/{HIGH_WATER_KEY}");
    let caught = std::panic::catch_unwind(|| assert_high_water_undeletable(policy, &object));
    assert!(
        caught.is_err(),
        "a wildcard delete over the bucket must fail"
    );
}
