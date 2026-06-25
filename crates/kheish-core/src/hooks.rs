use anyhow::Result;
use async_trait::async_trait;
use kheish_types::HookDispatchOutcome;
use kheish_types::HookInvocation;

/// One runtime-capable dispatcher for Kheish lifecycle hooks.
#[async_trait]
pub trait HookDispatcher: Send + Sync {
    /// Executes every hook that matches the provided invocation and returns the aggregated outcome.
    async fn dispatch(&self, invocation: HookInvocation) -> Result<HookDispatchOutcome>;
}

/// A no-op dispatcher used when hooks are disabled or unavailable.
#[derive(Debug, Default)]
pub struct NoopHookDispatcher;

#[async_trait]
impl HookDispatcher for NoopHookDispatcher {
    async fn dispatch(&self, _invocation: HookInvocation) -> Result<HookDispatchOutcome> {
        Ok(HookDispatchOutcome::default())
    }
}
