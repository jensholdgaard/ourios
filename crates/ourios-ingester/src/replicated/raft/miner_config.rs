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
