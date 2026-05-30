use copro_api::async_trait;
use copro_api::error::Result;
use copro_api::request::GenerateRequest;
use std::sync::Arc;

/// Mutates a model request immediately before it is submitted.
#[async_trait]
pub trait RequestInjector: Send + Sync {
    async fn prepare_request(&self, request: &mut GenerateRequest) -> Result<()>;
}

/// Runs request injectors in order.
#[derive(Clone, Default)]
pub struct RequestPipeline {
    injectors: Vec<Arc<dyn RequestInjector>>,
}

impl RequestPipeline {
    pub fn new(injectors: Vec<Arc<dyn RequestInjector>>) -> Self {
        Self { injectors }
    }

    pub fn len(&self) -> usize {
        self.injectors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.injectors.is_empty()
    }

    pub fn push(&mut self, injector: Arc<dyn RequestInjector>) {
        self.injectors.push(injector);
    }

    pub fn with_injector(mut self, injector: Arc<dyn RequestInjector>) -> Self {
        self.push(injector);
        self
    }

    pub async fn prepare_request(&self, request: &mut GenerateRequest) -> Result<()> {
        for injector in &self.injectors {
            injector.prepare_request(request).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl RequestInjector for RequestPipeline {
    async fn prepare_request(&self, request: &mut GenerateRequest) -> Result<()> {
        RequestPipeline::prepare_request(self, request).await
    }
}
