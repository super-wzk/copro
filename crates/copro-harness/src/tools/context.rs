use copro_agent::CancellationToken;
use copro_api::message::ToolCallId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

type BoxToolUpdateFuture = Pin<Box<dyn Future<Output = ()> + Send>>;
type ToolUpdateFn = dyn Fn(ToolUpdate) -> BoxToolUpdateFuture + Send + Sync;

/// Runtime context passed to an in-process tool call.
#[derive(Clone)]
pub struct ToolContext {
    call_id: ToolCallId,
    tool_name: String,
    cancel: CancellationToken,
    slots: ToolSlots,
    sequence: Arc<AtomicU64>,
}

impl ToolContext {
    pub fn new(
        call_id: impl Into<ToolCallId>,
        tool_name: impl Into<String>,
        cancel: CancellationToken,
        slots: ToolSlots,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            cancel,
            slots,
            sequence: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn without_slots(
        call_id: impl Into<ToolCallId>,
        tool_name: impl Into<String>,
        cancel: CancellationToken,
    ) -> Self {
        Self::new(call_id, tool_name, cancel, ToolSlots::default())
    }

    pub fn slots(&self) -> &ToolSlots {
        &self.slots
    }

    pub fn call_id(&self) -> &ToolCallId {
        &self.call_id
    }

    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancel
    }

    pub fn slot<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.slots.get::<T>()
    }

    pub async fn emit<U>(&self, update: U) -> Result<(), String>
    where
        U: IntoToolUpdate,
    {
        let Some(slot) = self.slot::<ToolUpdateSlot>() else {
            return Ok(());
        };
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        let update = update.into_update(self, sequence)?;
        slot.emit(update).await;
        Ok(())
    }
}

/// Type-erased host capabilities made available to tools.
#[derive(Clone, Default)]
pub struct ToolSlots {
    values: Arc<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>,
}

impl ToolSlots {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with<T>(mut self, value: T) -> Self
    where
        T: Send + Sync + 'static,
    {
        self.insert(value);
        self
    }

    pub fn insert<T>(&mut self, value: T)
    where
        T: Send + Sync + 'static,
    {
        Arc::make_mut(&mut self.values).insert(TypeId::of::<T>(), Arc::new(value));
    }

    pub fn get<T>(&self) -> Option<Arc<T>>
    where
        T: Send + Sync + 'static,
    {
        self.values
            .get(&TypeId::of::<T>())
            .cloned()
            .and_then(|value| value.downcast::<T>().ok())
    }
}

/// A type-erased tool execution update for UI, logs, or progress displays.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolUpdate {
    pub call_id: ToolCallId,
    pub tool_name: String,
    pub sequence: u64,
    pub kind: String,
    pub payload: Value,
}

/// Raw update parts for callers that already work with erased update payloads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolUpdateParts {
    pub kind: String,
    pub payload: Value,
}

impl ToolUpdateParts {
    pub fn new(kind: impl Into<String>, payload: Value) -> Self {
        Self {
            kind: kind.into(),
            payload,
        }
    }
}

/// Typed helper for tool update payloads.
pub trait ToolUpdatePayload: Serialize + Send + 'static {
    const KIND: &'static str;
}

/// Converts typed or erased update payloads into a runtime [`ToolUpdate`].
pub trait IntoToolUpdate: Send + 'static {
    fn into_update(self, context: &ToolContext, sequence: u64) -> Result<ToolUpdate, String>;
}

impl IntoToolUpdate for ToolUpdateParts {
    fn into_update(self, context: &ToolContext, sequence: u64) -> Result<ToolUpdate, String> {
        Ok(ToolUpdate {
            call_id: context.call_id.clone(),
            tool_name: context.tool_name.clone(),
            sequence,
            kind: self.kind,
            payload: self.payload,
        })
    }
}

impl<T> IntoToolUpdate for T
where
    T: ToolUpdatePayload,
{
    fn into_update(self, context: &ToolContext, sequence: u64) -> Result<ToolUpdate, String> {
        let payload = serde_json::to_value(&self).map_err(|error| error.to_string())?;
        Ok(ToolUpdate {
            call_id: context.call_id.clone(),
            tool_name: context.tool_name.clone(),
            sequence,
            kind: T::KIND.to_string(),
            payload,
        })
    }
}

/// Host-provided update callback slot.
#[derive(Clone)]
pub struct ToolUpdateSlot {
    emit: Arc<ToolUpdateFn>,
}

impl ToolUpdateSlot {
    pub fn new<F, Fut>(emit: F) -> Self
    where
        F: Fn(ToolUpdate) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Self {
            emit: Arc::new(move |update| Box::pin(emit(update))),
        }
    }

    pub async fn emit(&self, update: ToolUpdate) {
        (self.emit)(update).await;
    }
}
