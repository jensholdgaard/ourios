//! Reconfiguring a live cluster (RFC 0065 §6.5, "Miner configuration"):
//! every field but the tree's shape takes effect on the tree as it
//! stands.

use std::fmt;

use ourios_config::MinerConfig;

use super::MinerCluster;

/// The [`MinerConfig`] fields that shape the Drain tree itself. A
/// snapshot's routes are only valid at the shape it was mined at, so a
/// live cluster keeps the shape it started with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeShape {
    pub prefix_depth: u8,
    pub max_node_children: u16,
}

impl TreeShape {
    #[must_use]
    pub fn of(config: &MinerConfig) -> Self {
        Self {
            prefix_depth: config.prefix_depth,
            max_node_children: config.max_node_children,
        }
    }
}

/// [`MinerCluster::reconfigure`] was handed a configuration with a
/// different [`TreeShape`]; the cluster is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeShapeChanged {
    pub current: TreeShape,
    pub requested: TreeShape,
}

impl fmt::Display for TreeShapeChanged {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "miner reconfiguration changes the tree shape from {:?} to {:?}",
            self.current, self.requested
        )
    }
}

impl std::error::Error for TreeShapeChanged {}

impl MinerCluster {
    /// Make `config` the cluster default and apply it to every live
    /// tenant from the next ingested line, on the tree as it stands.
    /// Tenants allocated later start with it too. A tenant with a
    /// per-tenant override from [`Self::with_tenant_config`] keeps its
    /// override.
    ///
    /// Nothing already mined is rewritten, so the state is the one a
    /// restore of the same snapshot under `config` holds:
    ///
    /// - `max_templates` below a tenant's current count removes no
    ///   template; the tenant is at its ceiling, so a line that would
    ///   mint takes the `template_ceiling` parse failure with its body
    ///   retained, while existing templates keep attaching and widening.
    /// - A change of `upstream_templates` keeps the adopted-template map
    ///   and the stored associations: adopted ids stay valid, still count
    ///   against the ceiling and still converge with a mined leaf of the
    ///   same shape; the new mode decides only how later lines are read.
    ///
    /// # Errors
    ///
    /// [`TreeShapeChanged`] when `config`'s `prefix_depth` or
    /// `max_node_children` differs from the current default's; nothing
    /// is changed.
    pub fn reconfigure(&mut self, config: MinerConfig) -> Result<(), TreeShapeChanged> {
        let current = TreeShape::of(&self.config);
        match TreeShape::of(&config) {
            requested if requested != current => Err(TreeShapeChanged { current, requested }),
            _ => {
                self.config = config;
                self.tenants
                    .iter_mut()
                    .filter(|(tenant, _)| !self.tenant_overrides.contains_key(*tenant))
                    .for_each(|(_, state)| state.config = config);
                Ok(())
            }
        }
    }
}
