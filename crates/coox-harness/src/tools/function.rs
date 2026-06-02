use super::output::ToolOutput;
use super::tool::{ErasedTool, Tool};
use crate::tools::ToolContext;
use copro_agent::ToolExecutionPolicy;
use copro_api::async_trait;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;

type BoxToolFuture<Output> = Pin<Box<dyn Future<Output = Result<Output, String>> + Send>>;
type FnToolHandler<Input, Output> =
    dyn Fn(Input, ToolContext) -> BoxToolFuture<Output> + Send + Sync;

/// Builder for constructing an erased [`FnTool`] with optional configuration.
///
/// Created via [`ToolBuilder::new`] or the [`tool!`] macro.
pub struct ToolBuilder<Input, Output, F, Fut> {
    name: String,
    description: String,
    handler: F,
    execution_policy: ToolExecutionPolicy,
    _marker: PhantomData<(Input, Output, Fut)>,
}

impl<Input, Output, F, Fut> ToolBuilder<Input, Output, F, Fut>
where
    Input: DeserializeOwned + JsonSchema + Send + 'static,
    Output: ToolOutput + Send + 'static,
    F: Fn(Input, ToolContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Output, String>> + Send + 'static,
{
    pub fn new(name: impl Into<String>, description: impl Into<String>, handler: F) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            handler,
            execution_policy: ToolExecutionPolicy::Serial,
            _marker: PhantomData,
        }
    }

    /// Set the execution policy for this tool.
    pub fn policy(mut self, execution_policy: ToolExecutionPolicy) -> Self {
        self.execution_policy = execution_policy;
        self
    }

    /// Build the tool, returning an erased reference.
    pub fn build(self) -> Arc<dyn ErasedTool> {
        let handler = Arc::new(
            move |input: Input, context: ToolContext| -> BoxToolFuture<Output> {
                Box::pin((self.handler)(input, context))
            },
        );
        Arc::new(FnTool {
            name: self.name,
            description: self.description,
            execution_policy: self.execution_policy,
            handler,
            _marker: PhantomData,
        })
    }
}

/// Create a [`ToolBuilder`] with the minimal required fields.
///
/// Tool implementation backed by an async function or closure.
pub struct FnTool<Input, Output> {
    name: String,
    description: String,
    execution_policy: ToolExecutionPolicy,
    handler: Arc<FnToolHandler<Input, Output>>,
    _marker: PhantomData<fn(Input) -> Output>,
}

impl<Input, Output> FnTool<Input, Output> {
    pub fn new<F, Fut>(name: impl Into<String>, description: impl Into<String>, handler: F) -> Self
    where
        Input: 'static,
        Output: 'static,
        F: Fn(Input, ToolContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Output, String>> + Send + 'static,
    {
        let handler = Arc::new(
            move |input: Input, context: ToolContext| -> BoxToolFuture<Output> {
                Box::pin(handler(input, context))
            },
        );

        Self {
            name: name.into(),
            description: description.into(),
            execution_policy: ToolExecutionPolicy::Serial,
            handler,
            _marker: PhantomData,
        }
    }

    pub fn with_execution_policy(mut self, execution_policy: ToolExecutionPolicy) -> Self {
        self.execution_policy = execution_policy;
        self
    }
}

impl<Input, Output> Clone for FnTool<Input, Output> {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            description: self.description.clone(),
            execution_policy: self.execution_policy,
            handler: Arc::clone(&self.handler),
            _marker: PhantomData,
        }
    }
}

#[async_trait]
impl<Input, Output> Tool for FnTool<Input, Output>
where
    Input: DeserializeOwned + JsonSchema + Send + 'static,
    Output: ToolOutput + Send + 'static,
{
    type Input = Input;
    type Output = Output;

    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn execution_policy(&self) -> ToolExecutionPolicy {
        self.execution_policy
    }

    async fn call(&self, input: Self::Input, context: ToolContext) -> Result<Self::Output, String> {
        (self.handler)(input, context).await
    }
}

/// Create an erased [`FnTool`] from an async function or closure.
///
/// ```ignore
/// // Builder API (full IDE support):
/// ToolBuilder::new("echo", "Echo a message.", echo)
///     .policy(ToolExecutionPolicy::Parallel)
///     .build()
///
/// // Macro shorthand:
/// tool!("echo", "Echo a message.", echo);
/// tool!("echo", "Echo a message.", echo, policy = ToolExecutionPolicy::Parallel);
/// ```
#[macro_export]
macro_rules! tool {
    ($name:expr, $desc:expr, $handler:expr) => {
        $crate::tools::ToolBuilder::new($name, $desc, $handler).build()
    };
    ($name:expr, $desc:expr, $handler:expr, $($key:ident = $value:expr),* $(,)?) => {
        $crate::tools::ToolBuilder::new($name, $desc, $handler)
            $( .$key($value) )*
            .build()
    };
}
