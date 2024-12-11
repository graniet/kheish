use crate::config::AgentsConfig;

/// Manages the lifecycle and coordination of AI agents
#[derive(Debug)]
pub struct AgentsManager {
    /// Configuration settings for the agents
    pub _config: AgentsConfig,
}

impl AgentsManager {
    /// Creates a new AgentsManager with the provided configuration
    ///
    /// # Arguments
    /// * `config` - Configuration settings for the agents
    ///
    /// # Returns
    /// * `Self` - New AgentsManager instance
    pub fn new(config: AgentsConfig) -> Self {
        AgentsManager { _config: config }
    }
}
