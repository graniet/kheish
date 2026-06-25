//! Generic response-output plugins for Kheish.
//!
//! Input plugins only inject work into the runtime. Output plugins are
//! responsible for routing finalized responses back to the outside world.

use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use kheish_types::{
    AttachmentRef, ContentPart, ConversationKey, ReplyHandle, normalize_reply_targets,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

/// Describes a normalized response emitted by the runtime.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResponseEnvelope {
    pub conversation: ConversationKey,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reply_targets: Vec<ReplyHandle>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<ReplyHandle>,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<ContentPart>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<AttachmentRef>,
    pub metadata: Value,
}

/// Describes an output plugin.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputManifest {
    pub name: String,
    pub version: String,
    pub description: String,
}

/// Output plugin interface.
#[async_trait]
pub trait OutputPlugin: Send + Sync {
    /// Returns the output plugin manifest.
    fn manifest(&self) -> OutputManifest;

    /// Delivers a response envelope.
    async fn deliver(&self, response: ResponseEnvelope) -> Result<()>;
}

/// Coordinates output plugins.
#[derive(Default)]
pub struct OutputHost {
    plugins: Vec<Arc<dyn OutputPlugin>>,
}

impl OutputHost {
    /// Creates an empty output host.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers an output plugin.
    pub fn register<P>(&mut self, plugin: P)
    where
        P: OutputPlugin + 'static,
    {
        self.plugins.push(Arc::new(plugin));
    }

    /// Returns plugin manifests.
    pub fn manifests(&self) -> Vec<OutputManifest> {
        self.plugins
            .iter()
            .map(|plugin| plugin.manifest())
            .collect()
    }

    /// Delivers a response to every registered output plugin.
    pub async fn deliver(&self, response: ResponseEnvelope) -> Result<()> {
        if self.plugins.is_empty() {
            return Err(anyhow!("no output plugin registered"));
        }
        let targets =
            normalize_reply_targets(response.reply.clone(), response.reply_targets.clone());
        if targets.is_empty() {
            let mut deliveries = JoinSet::new();
            for plugin in &self.plugins {
                let plugin = plugin.clone();
                let response = response.clone();
                deliveries.spawn(async move { plugin.deliver(response).await });
            }
            while let Some(result) = deliveries.join_next().await {
                result.map_err(|error| anyhow!("output delivery task failed: {error}"))??;
            }
            return Ok(());
        }
        let mut deliveries = JoinSet::new();
        let available_plugins = self
            .plugins
            .iter()
            .map(|plugin| plugin.manifest().name)
            .collect::<BTreeSet<_>>();
        let unmatched_plugins = targets
            .iter()
            .filter(|target| !available_plugins.contains(&target.plugin))
            .map(|target| target.plugin.clone())
            .collect::<BTreeSet<_>>();
        if !unmatched_plugins.is_empty() {
            return Err(anyhow!(
                "no output plugin matched the requested routes: {}",
                unmatched_plugins.into_iter().collect::<Vec<_>>().join(", ")
            ));
        }
        for target in targets {
            let narrowed = ResponseEnvelope {
                conversation: response.conversation.clone(),
                reply_targets: Vec::new(),
                reply: Some(target.clone()),
                content: response.content.clone(),
                parts: response.parts.clone(),
                artifacts: response.artifacts.clone(),
                metadata: response.metadata.clone(),
            };
            for plugin in &self.plugins {
                if plugin.manifest().name != target.plugin {
                    continue;
                }
                let plugin = plugin.clone();
                let narrowed = narrowed.clone();
                deliveries.spawn(async move {
                    plugin.deliver(narrowed).await?;
                    Ok::<usize, anyhow::Error>(1)
                });
            }
        }
        let mut delivered = 0usize;
        while let Some(result) = deliveries.join_next().await {
            delivered +=
                result.map_err(|error| anyhow!("output delivery task failed: {error}"))??;
        }
        if delivered == 0 {
            return Err(anyhow!("no output plugin matched the requested route"));
        }
        Ok(())
    }
}

/// In-memory output plugin used by tests and local harnesses.
pub struct MemoryOutputPlugin {
    manifest: OutputManifest,
    sender: mpsc::UnboundedSender<ResponseEnvelope>,
}

impl MemoryOutputPlugin {
    /// Creates a memory-backed output plugin and returns its receiver.
    pub fn new(name: impl Into<String>) -> (Self, mpsc::UnboundedReceiver<ResponseEnvelope>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (
            Self {
                manifest: OutputManifest {
                    name: name.into(),
                    version: "0.1.0".to_string(),
                    description: "In-memory response sink".to_string(),
                },
                sender,
            },
            receiver,
        )
    }
}

#[async_trait]
impl OutputPlugin for MemoryOutputPlugin {
    fn manifest(&self) -> OutputManifest {
        self.manifest.clone()
    }

    async fn deliver(&self, response: ResponseEnvelope) -> Result<()> {
        self.sender
            .send(response)
            .map_err(|_| anyhow!("output sink is closed"))
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use serde_json::Value;

    use super::{MemoryOutputPlugin, OutputHost, ResponseEnvelope};
    use kheish_types::{ConversationKey, ReplyHandle};

    #[tokio::test]
    async fn output_host_delivers_to_registered_plugins() -> Result<()> {
        let (plugin, mut receiver) = MemoryOutputPlugin::new("memory");
        let mut host = OutputHost::new();
        host.register(plugin);
        host.deliver(ResponseEnvelope {
            conversation: ConversationKey {
                session_id: "session-1".to_string(),
                thread_id: None,
            },
            reply_targets: Vec::new(),
            reply: None,
            content: "hello".to_string(),
            parts: Vec::new(),
            artifacts: Vec::new(),
            metadata: Value::Null,
        })
        .await?;

        let response = receiver.recv().await.expect("response should exist");
        assert_eq!(response.content, "hello");
        Ok(())
    }

    #[tokio::test]
    async fn output_host_routes_to_the_requested_plugin_only() -> Result<()> {
        let (plugin_a, mut receiver_a) = MemoryOutputPlugin::new("memory-a");
        let (plugin_b, mut receiver_b) = MemoryOutputPlugin::new("memory-b");
        let mut host = OutputHost::new();
        host.register(plugin_a);
        host.register(plugin_b);

        host.deliver(ResponseEnvelope {
            conversation: ConversationKey {
                session_id: "session-2".to_string(),
                thread_id: None,
            },
            reply_targets: Vec::new(),
            reply: Some(ReplyHandle {
                plugin: "memory-b".to_string(),
                address: "thread-1".to_string(),
            }),
            content: "targeted".to_string(),
            parts: Vec::new(),
            artifacts: Vec::new(),
            metadata: Value::Null,
        })
        .await?;

        assert!(receiver_a.try_recv().is_err());
        let response = receiver_b.recv().await.expect("response should exist");
        assert_eq!(response.content, "targeted");
        Ok(())
    }

    #[tokio::test]
    async fn output_host_fans_out_to_multiple_reply_targets() -> Result<()> {
        let (plugin_a, mut receiver_a) = MemoryOutputPlugin::new("memory-a");
        let (plugin_b, mut receiver_b) = MemoryOutputPlugin::new("memory-b");
        let mut host = OutputHost::new();
        host.register(plugin_a);
        host.register(plugin_b);

        host.deliver(ResponseEnvelope {
            conversation: ConversationKey {
                session_id: "session-3".to_string(),
                thread_id: None,
            },
            reply_targets: vec![
                ReplyHandle {
                    plugin: "memory-a".to_string(),
                    address: "a-1".to_string(),
                },
                ReplyHandle {
                    plugin: "memory-b".to_string(),
                    address: "b-1".to_string(),
                },
            ],
            reply: None,
            content: "fanout".to_string(),
            parts: Vec::new(),
            artifacts: Vec::new(),
            metadata: Value::Null,
        })
        .await?;

        assert_eq!(
            receiver_a
                .recv()
                .await
                .expect("response should exist")
                .reply,
            Some(ReplyHandle {
                plugin: "memory-a".to_string(),
                address: "a-1".to_string(),
            })
        );
        assert_eq!(
            receiver_b
                .recv()
                .await
                .expect("response should exist")
                .reply,
            Some(ReplyHandle {
                plugin: "memory-b".to_string(),
                address: "b-1".to_string(),
            })
        );
        Ok(())
    }

    #[tokio::test]
    async fn output_host_rejects_unknown_reply_targets_even_when_one_target_matches() {
        let (plugin, _receiver) = MemoryOutputPlugin::new("memory");
        let mut host = OutputHost::new();
        host.register(plugin);

        let error = host
            .deliver(ResponseEnvelope {
                conversation: ConversationKey {
                    session_id: "session-4".to_string(),
                    thread_id: None,
                },
                reply_targets: vec![
                    ReplyHandle {
                        plugin: "memory".to_string(),
                        address: "known".to_string(),
                    },
                    ReplyHandle {
                        plugin: "missing".to_string(),
                        address: "unknown".to_string(),
                    },
                ],
                reply: None,
                content: "fanout".to_string(),
                parts: Vec::new(),
                artifacts: Vec::new(),
                metadata: Value::Null,
            })
            .await
            .expect_err("unknown reply target should fail delivery");

        assert!(
            error
                .to_string()
                .contains("no output plugin matched the requested routes: missing")
        );
    }
}
