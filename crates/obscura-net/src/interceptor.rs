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
    /// Return a request-scoped Authorization value for a challenge, if handled.
    async fn authenticate(&self, _url: &url::Url, _method: &str,
        _headers: &HashMap<String, String>, _challenge: &str) -> Option<String> { None }

    async fn intercept_response(&self, url: &url::Url, method: &str, status: u16,
        headers: HashMap<String, String>, body: ResponseBodyFuture) -> Result<ResponseInterception, String>;
}

/// Apply cookies introduced by a response-stage override. The transport has
/// already applied the original response's cookies before the pause.
pub fn apply_overridden_response_cookies(
    jar: &crate::cookies::CookieJar,
    url: &url::Url,
    original: Option<&str>,
    headers: &HashMap<String, String>,
) {
    let updated = headers.get("set-cookie").map(String::as_str);
    if updated != original {
        if let Some(cookies) = updated {
            for cookie in cookies.split('\n') {
                jar.set_cookie(cookie, url);
            }
        }
    }
}

#[cfg(test)]
mod response_cookie_tests {
    use super::*;

    #[test]
    fn applies_each_cookie_introduced_by_response_override() {
        let jar = crate::cookies::CookieJar::new();
        let url = url::Url::parse("https://example.test/").unwrap();
        jar.set_cookie("wire=1; Path=/", &url);
        let headers = HashMap::from([("set-cookie".to_string(), "first=2; Path=/\nsecond=3; Path=/".to_string())]);
        apply_overridden_response_cookies(&jar, &url, Some("wire=1; Path=/"), &headers);
        let cookies = jar.get_cookie_header(&url);
        for cookie in ["wire=1", "first=2", "second=3"] {
            assert!(cookies.contains(cookie), "missing {cookie}: {cookies}");
        }
    }
}
