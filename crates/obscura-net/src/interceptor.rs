use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::client::{RequestInfo, Response};

pub enum InterceptAction {
    Continue,
    Block,
    Fulfill(Response),
    ModifyHeaders(HashMap<String, String>),
}

#[async_trait::async_trait]
pub trait RequestInterceptor {
    async fn intercept(&self, request: &RequestInfo) -> InterceptAction;
}

/// A bounded transport body whose reader can be shared by a paused CDP
/// response and the eventual consumer. The future is not polled until the
/// client asks for the body or resumes the response.
pub type ResponseBodyFuture = Pin<Box<dyn Future<Output = Result<Vec<u8>, String>> + Send + 'static>>;

pub struct HeldResponseBody {
    reader: tokio::sync::Mutex<Option<ResponseBodyFuture>>,
    bytes: tokio::sync::OnceCell<Result<Arc<Vec<u8>>, String>>,
}

impl HeldResponseBody {
    pub fn new(reader: ResponseBodyFuture) -> Self {
        Self { reader: tokio::sync::Mutex::new(Some(reader)), bytes: tokio::sync::OnceCell::new() }
    }

    pub async fn bytes(&self) -> Result<Arc<Vec<u8>>, String> {
        self.bytes.get_or_init(|| async {
            let reader = self.reader.lock().await.take().ok_or_else(|| "response body reader is unavailable".to_string())?;
            reader.await.map(Arc::new)
        }).await.clone()
    }
}

pub struct ResponseInterception {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: ResponseBodyFuture,
}

#[async_trait::async_trait]
pub trait ResponseInterceptor: Send + Sync {
    async fn intercept_response(&self, url: &url::Url, method: &str, status: u16,
        headers: HashMap<String, String>, body: ResponseBodyFuture) -> Result<ResponseInterception, String>;
}
