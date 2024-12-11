mod fs_module;
mod module_manager;
pub mod rag_module;
mod sh_module;

use crate::core::rag::VectorStoreProvider;
pub use fs_module::*;
pub use module_manager::*;
pub use rag_module::*;
pub use sh_module::*;

pub struct ModuleAction {
    pub name: String,
    pub arg_count: usize,
    pub description: String,
}

impl std::fmt::Display for ModuleAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({} args) - {}", self.name, self.arg_count, self.description)
    }
}

#[async_trait::async_trait]
pub trait Module: std::fmt::Debug + Send + Sync {
    fn name(&self) -> &str;
    async fn handle_action(
        &self,
        vector_store: &mut dyn VectorStoreProvider,
        action: &str,
        params: &[String],
    ) -> Result<String, String>;
    fn get_actions(&self) -> Vec<ModuleAction>;
}
