//! A group's miner configuration (RFC 0065 §6.5): the body of a miner
//! configuration entry, algorithm 1, once `MinerConfig` validation has
//! accepted it.

use ourios_config::{MinerConfig, MinerConfigError, UpstreamTemplates};
use ourios_wal::MinerConfigV1;

/// A miner configuration entry's body that `MinerConfig` accepts, and
/// the configuration it is. Equal when the bodies are.
#[derive(Debug, Clone, Copy)]
pub struct GroupMinerConfig {
    body: MinerConfigV1,
    config: MinerConfig,
}

impl GroupMinerConfig {
    /// # Errors
    ///
    /// The [`MinerConfigError`] naming the first bound `body` breaks.
    pub fn new(body: MinerConfigV1) -> Result<Self, MinerConfigError> {
        let config = config_of(body)?;
        Ok(Self { body, config })
    }

    #[must_use]
    pub fn body(self) -> MinerConfigV1 {
        self.body
    }

    #[must_use]
    pub fn config(self) -> MinerConfig {
        self.config
    }

    /// Whether `other` keeps the tree's shape: `prefix_depth` and
    /// `max_node_children`, which a group's first configuration fixes.
    #[must_use]
    pub fn same_shape(self, other: Self) -> bool {
        (self.body.prefix_depth, self.body.max_node_children)
            == (other.body.prefix_depth, other.body.max_node_children)
    }
}

impl PartialEq for GroupMinerConfig {
    fn eq(&self, other: &Self) -> bool {
        self.body == other.body
    }
}

impl Eq for GroupMinerConfig {}

/// The miner configuration entry a leader whose own effective
/// configuration is `own` appends over `in_force`, the one in force
/// (§6.5): `own` with `in_force`'s tree shape, or with its own when none
/// is in force, and none when `in_force` already equals that.
#[must_use]
pub fn plan(own: &MinerConfig, in_force: Option<&MinerConfigV1>) -> Option<MinerConfigV1> {
    let mine = body_of(own);
    match in_force {
        None => Some(mine),
        Some(&in_force) => {
            let planned = MinerConfigV1 {
                prefix_depth: in_force.prefix_depth,
                max_node_children: in_force.max_node_children,
                ..mine
            };
            (planned != in_force).then_some(planned)
        }
    }
}

/// The body that carries `config`.
#[must_use]
pub fn body_of(config: &MinerConfig) -> MinerConfigV1 {
    let upstream_templates = match config.upstream_templates {
        UpstreamTemplates::Ignore => ourios_wal::UpstreamTemplates::Ignore,
        UpstreamTemplates::Observe => ourios_wal::UpstreamTemplates::Observe,
        UpstreamTemplates::Adopt => ourios_wal::UpstreamTemplates::Adopt,
    };
    MinerConfigV1 {
        similarity_threshold: config.similarity_threshold,
        similarity_floor: config.similarity_floor,
        param_byte_limit: config.param_byte_limit,
        prefix_depth: config.prefix_depth,
        max_node_children: config.max_node_children,
        max_templates: config.max_templates,
        max_line_tokens: config.max_line_tokens,
        upstream_templates,
        upstream_template_byte_limit: config.upstream_template_byte_limit,
        upstream_association_limit: config.upstream_association_limit,
    }
}

fn config_of(body: MinerConfigV1) -> Result<MinerConfig, MinerConfigError> {
    let upstream = match body.upstream_templates {
        ourios_wal::UpstreamTemplates::Ignore => UpstreamTemplates::Ignore,
        ourios_wal::UpstreamTemplates::Observe => UpstreamTemplates::Observe,
        ourios_wal::UpstreamTemplates::Adopt => UpstreamTemplates::Adopt,
    };
    Ok(MinerConfig::try_new_full(
        body.similarity_threshold,
        body.similarity_floor,
        body.param_byte_limit,
    )?
    .with_prefix_depth(body.prefix_depth)?
    .with_max_node_children(body.max_node_children)?
    .with_max_templates(body.max_templates)?
    .with_max_line_tokens(body.max_line_tokens)?
    .with_upstream_templates(upstream)
    .with_upstream_template_byte_limit(body.upstream_template_byte_limit)
    .with_upstream_association_limit(body.upstream_association_limit))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn own(threshold: f32, prefix_depth: u8) -> MinerConfig {
        MinerConfig::try_new(threshold, 256)
            .and_then(|c| c.with_prefix_depth(prefix_depth))
            .expect("valid")
    }

    #[test]
    fn with_none_in_force_the_leader_appends_its_own() {
        let mine = own(0.7, 3);
        assert_eq!(plan(&mine, None), Some(body_of(&mine)));
    }

    #[test]
    fn a_dynamic_field_that_differs_appends_the_leaders_with_the_shape_in_force() {
        let in_force = MinerConfigV1 {
            max_node_children: 7,
            ..body_of(&own(0.7, 2))
        };
        let leader = own(0.8, 4);
        let planned = plan(&leader, Some(&in_force)).expect("appended");
        let expected = MinerConfigV1 {
            prefix_depth: 2,
            max_node_children: 7,
            ..body_of(&leader)
        };
        assert_eq!(planned, expected, "the leader's, in the shape in force");
        assert_eq!(plan(&leader, Some(&planned)), None, "once in force");
    }

    #[test]
    fn a_shape_that_differs_alone_appends_nothing() {
        let in_force = body_of(&own(0.7, 2));
        let deeper = own(0.7, 5).with_max_node_children(9).expect("valid");
        assert_eq!(plan(&deeper, Some(&in_force)), None);
    }

    /// Fields compare by their bits, as the entry does.
    #[test]
    fn fields_compare_by_their_bits() {
        let in_force = MinerConfigV1 {
            similarity_floor: f32::NAN,
            ..body_of(&own(0.7, 2))
        };
        let mine = MinerConfig {
            similarity_floor: f32::NAN,
            ..own(0.7, 2)
        };
        assert_eq!(plan(&mine, Some(&in_force)), None, "NaN is its own bits");
        let other = MinerConfig {
            upstream_association_limit: 5,
            ..mine
        };
        let planned = plan(&other, Some(&in_force)).expect("appended");
        assert_eq!(planned.upstream_association_limit, 5);
    }
}
