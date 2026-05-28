use super::output::ToolOutput;
use super::tool::{ErasedTool, Tool};
use copro_api::async_trait;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;

type BoxToolFuture<Output> = Pin<Box<dyn Future<Output = Result<Output, String>> + Send>>;
type FnToolHandler<Input, Output> = dyn Fn(Input) -> BoxToolFuture<Output> + Send + Sync;

/// Tool implementation backed by an async function or closure.
pub struct FnTool<Input, Output> {
    name: String,
    description: String,
    handler: Arc<FnToolHandler<Input, Output>>,
    _marker: PhantomData<fn(Input) -> Output>,
}

impl<Input, Output> FnTool<Input, Output> {
    pub fn new<F, Fut>(name: impl Into<String>, description: impl Into<String>, handler: F) -> Self
    where
        Input: 'static,
        Output: 'static,
        F: Fn(Input) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Output, String>> + Send + 'static,
    {
        let handler =
            Arc::new(move |input: Input| -> BoxToolFuture<Output> { Box::pin(handler(input)) });

        Self {
            name: name.into(),
            description: description.into(),
            handler,
            _marker: PhantomData,
        }
    }
}

impl<Input, Output> Clone for FnTool<Input, Output> {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            description: self.description.clone(),
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

    async fn call(&self, input: Self::Input) -> Result<Self::Output, String> {
        (self.handler)(input).await
    }
}

/// Create an erased [`FnTool`] from an async function or closure.
pub fn tool_fn<Input, Output, F, Fut>(
    name: impl Into<String>,
    description: impl Into<String>,
    handler: F,
) -> Arc<dyn ErasedTool>
where
    Input: DeserializeOwned + JsonSchema + Send + 'static,
    Output: ToolOutput + Send + 'static,
    F: Fn(Input) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Output, String>> + Send + 'static,
{
    Arc::new(FnTool::<Input, Output>::new(name, description, handler))
}
