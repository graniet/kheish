use crate::modules::{
    FileSystemModule, HttpModule, MemoriesModule, Module, ShModule, SshModule, VectorStoreModule,
};
use tracing::debug;

/// Manages the loading and access of modules in the system
#[derive(Debug)]
pub struct ModulesManager {
    /// Vector containing the loaded module instances
    pub modules: Vec<Box<dyn Module>>,
}

impl ModulesManager {
    /// Creates a new ModulesManager instance by loading modules from configuration
    ///
    /// # Arguments
    /// * `mods_config` - Vector of module configurations specifying which modules to load
    ///
    /// # Returns
    /// * `ModulesManager` - New manager instance with loaded modules
    pub fn new(mods_config: Vec<crate::config::ModuleConfig>) -> Self {
        let modules = mods_config
            .into_iter()
            .filter_map(|mc| match mc.name.as_str() {
                "fs" => {
                    if let Some(version) = &mc.version {
                        debug!("Loading fs module version {}", version);
                    }
                    Some(Box::new(FileSystemModule) as Box<dyn Module>)
                }
                "ssh" => Some(Box::new(SshModule) as Box<dyn Module>),
                "http" => Some(Box::new(HttpModule::new()) as Box<dyn Module>),
                "sh" => mc.config.map(|conf| {
                    let allowed_commands = conf
                        .get("allowed_commands")
                        .and_then(|v| v.as_array())
                        .map(|cmds| {
                            cmds.iter()
                                .filter_map(|val| val.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default();
                    Box::new(ShModule::new(allowed_commands)) as Box<dyn Module>
                }),
                "rag" => Some(Box::new(VectorStoreModule) as Box<dyn Module>),
                "memories" => Some(Box::new(MemoriesModule) as Box<dyn Module>),
                _ => {
                    eprintln!("Unknown module: {}", mc.name);
                    None
                }
            })
            .collect();

        debug!("Loaded modules: {:?}", modules);
        ModulesManager { modules }
    }

    /// Creates a new ModulesManager instance with all available modules loaded
    ///
    /// # Returns
    /// * `ModulesManager` - New manager instance containing all supported modules
    pub fn new_with_all_modules() -> Self {
        let modules = vec![
            Box::new(FileSystemModule) as Box<dyn Module>,
            Box::new(SshModule) as Box<dyn Module>,
            Box::new(HttpModule::new()) as Box<dyn Module>,
            Box::new(ShModule::new(vec![])) as Box<dyn Module>,
            Box::new(VectorStoreModule) as Box<dyn Module>,
            Box::new(MemoriesModule) as Box<dyn Module>,
        ];
        ModulesManager { modules }
    }

    /// Retrieves a reference to a loaded module by its name
    ///
    /// # Arguments
    /// * `name` - Name of the module to retrieve
    ///
    /// # Returns
    /// * `Option<&dyn Module>` - Reference to the module if found, None otherwise
    pub fn get_module(&self, name: &str) -> Option<&dyn Module> {
        self.modules.iter().find(|m| m.name() == name).map(|m| &**m)
    }
}
