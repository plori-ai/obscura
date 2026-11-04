use std::cell::RefCell;
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::sync::Arc;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};

use deno_core::error::ModuleLoaderError;
use deno_core::ModuleLoadOptions;
use deno_core::ModuleLoadReferrer;
use deno_core::ModuleLoadResponse;
use deno_core::ModuleLoader;
use deno_core::ModuleSource;
use deno_core::ModuleSourceCode;
use deno_core::ModuleSpecifier;

use deno_error::JsErrorBox;

use crate::import_map::ImportMap;
use crate::ops::{ObscuraState, JsNetworkEvent, StoredNetworkResponseBody, FetchNetworkGuard, record_network_completion};

/// Observable network activity for ES-module graphs.
///
/// deno_core keeps dynamic-import state inside its private module map. The
/// browser lifecycle still needs to distinguish a genuinely idle page from a
/// graph whose fetch future is being advanced in short event-loop slices. A
/// loader-owned counter provides that signal without treating unrelated
/// fetch/XHR analytics as render-blocking work.
#[derive(Debug, Default)]
pub(crate) struct ModuleLoadActivity {
    pending: std::sync::atomic::AtomicUsize,
    last_activity: std::sync::Mutex<Option<std::time::Instant>>,
}

impl ModuleLoadActivity {
    fn begin(self: &Arc<Self>) -> ModuleLoadGuard {
        self.pending
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        *self
            .last_activity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(std::time::Instant::now());
        ModuleLoadGuard(self.clone())
    }

    pub(crate) fn is_pending_or_recent(&self, grace: std::time::Duration) -> bool {
        if self.pending.load(std::sync::atomic::Ordering::Relaxed) != 0 {
            return true;
        }
        self.last_activity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some_and(|last| last.elapsed() <= grace)
    }
}

struct ModuleLoadGuard(Arc<ModuleLoadActivity>);

impl Drop for ModuleLoadGuard {
    fn drop(&mut self) {
        let previous = self
            .0
            .pending
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        debug_assert!(previous > 0, "module load activity counter underflow");
        *self
            .0
            .last_activity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(std::time::Instant::now());
    }
}

pub struct ObscuraModuleLoader {
    pub base_url: String,
    /// Proxy URL threaded through to every dynamic ES-module fetch (#139).
    /// `None` keeps the pre-#139 direct-connection behaviour for callers
    /// that haven't been updated.
    pub proxy_url: Option<String>,
    /// The owning page's network context. Production runtimes always install
    /// this so every module in a graph uses the same cookie jar, configured
    /// identity, redirect/security policy, interception, and callbacks as the
    /// entry module. Directly-constructed standalone loaders remain supported.
    page_state: Option<Weak<RefCell<ObscuraState>>>,
    /// Directly-constructed loaders still use Obscura's network policy and
    /// connection pool; they simply have an isolated cookie jar.
    standalone_client: Option<Arc<obscura_net::ObscuraHttpClient>>,
    import_map: Rc<RefCell<ImportMap>>,
    activity: Arc<ModuleLoadActivity>,
    /// Canonical and requested specifiers fetched into deno_core's module map.
    /// The runtime uses a cursor into this append-only list to associate a
    /// prepared root with the dependencies that its successful evaluation also
    /// evaluates.
    loaded_specifiers: Rc<RefCell<Vec<String>>>,
}

impl ObscuraModuleLoader {
    pub fn new(base_url: &str) -> Self {
        Self::with_proxy(base_url, None)
    }

    pub fn with_proxy(base_url: &str, proxy_url: Option<String>) -> Self {
        let import_map = Rc::new(RefCell::new(ImportMap::default()));
        Self::with_proxy_and_import_map(base_url, proxy_url, import_map)
    }

    fn with_proxy_and_import_map(
        base_url: &str,
        proxy_url: Option<String>,
        import_map: Rc<RefCell<ImportMap>>,
    ) -> Self {
        let standalone_client = Arc::new(obscura_net::ObscuraHttpClient::with_options(
            Arc::new(obscura_net::CookieJar::new()),
            proxy_url.as_deref(),
        ));
        ObscuraModuleLoader {
            base_url: base_url.to_string(),
            proxy_url,
            page_state: None,
            standalone_client: Some(standalone_client),
            import_map,
            activity: Arc::new(ModuleLoadActivity::default()),
            loaded_specifiers: Rc::new(RefCell::new(Vec::new())),
        }
    }

    pub(crate) fn with_page_state(
        base_url: &str,
        proxy_url: Option<String>,
        page_state: &Rc<RefCell<ObscuraState>>,
        import_map: Rc<RefCell<ImportMap>>,
    ) -> Self {
        ObscuraModuleLoader {
            base_url: base_url.to_string(),
            proxy_url,
            page_state: Some(Rc::downgrade(page_state)),
            standalone_client: None,
            import_map,
            activity: Arc::new(ModuleLoadActivity::default()),
            loaded_specifiers: Rc::new(RefCell::new(Vec::new())),
        }
    }

    pub(crate) fn activity(&self) -> Arc<ModuleLoadActivity> {
        self.activity.clone()
    }

    pub(crate) fn loaded_specifiers(&self) -> Rc<RefCell<Vec<String>>> {
        self.loaded_specifiers.clone()
    }
}

fn io_err(msg: String) -> ModuleLoaderError {
    JsErrorBox::from_err(std::io::Error::new(std::io::ErrorKind::Other, msg))
}

fn javascript_mime(mime: &data_url::mime::Mime) -> bool {
    matches!((mime.type_.as_str(), mime.subtype.as_str()),
        ("application", "ecmascript" | "javascript" | "x-ecmascript" | "x-javascript") |
        ("text", "ecmascript" | "javascript" | "javascript1.0" | "javascript1.1" |
            "javascript1.2" | "javascript1.3" | "javascript1.4" | "javascript1.5" |
            "jscript" | "livescript" | "x-ecmascript" | "x-javascript"))
}

fn local_module_source(url: &str, state: Option<&Weak<RefCell<ObscuraState>>>) -> Result<Vec<u8>, String> {
    const MAX_MODULE_BYTES: usize = 32 * 1024 * 1024;
    if url.starts_with("data:") {
        let data = data_url::DataUrl::process(url).map_err(|e| format!("Invalid data module URL: {e:?}"))?;
        if !javascript_mime(data.mime_type()) { return Err("Module URL does not have a JavaScript MIME type".into()); }
        let mut bytes = Vec::new();
        data.decode(|chunk| {
            if bytes.len().saturating_add(chunk.len()) > MAX_MODULE_BYTES { return Err(()); }
            bytes.extend_from_slice(chunk);
            Ok(())
        }).map_err(|_| "Invalid or oversized data module".to_string())?;
        Ok(bytes)
    } else {
        let mut key = ModuleSpecifier::parse(url).map_err(|e| e.to_string())?;
        key.set_fragment(None);
        let state = state.and_then(Weak::upgrade).ok_or("Blob URL has no owning page")?;
        let state = state.try_borrow().map_err(|_| "Blob URL store is busy")?;
        let (mime, bytes) = state.blob_urls.get(key.as_str()).ok_or("Blob URL not found or revoked")?;
        let mime = mime.parse::<data_url::mime::Mime>().map_err(|_| "Invalid blob MIME type")?;
        if !javascript_mime(&mime) { return Err("Blob URL does not have a JavaScript MIME type".into()); }
        if bytes.len() > MAX_MODULE_BYTES { return Err("Blob module is too large".into()); }
        Ok(bytes.clone())
    }
}

impl ModuleLoader for ObscuraModuleLoader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _kind: deno_core::ResolutionKind,
    ) -> Result<ModuleSpecifier, ModuleLoaderError> {
        // deno_core represents the root passed to load_side_es_module with a
        // synthetic "." referrer. A browser resolves <script type=module src>
        // as a resource URL before it starts a graph; the document import map
        // must not remap that root URL.
        if referrer == "." {
            return deno_core::resolve_import(specifier, &self.base_url)
                .map_err(|error| io_err(error.to_string()));
        }

        let base = if referrer.is_empty()
            || referrer.starts_with('<')
            || referrer == "about:blank"
            || referrer == "undefined"
        {
            &self.base_url
        } else {
            referrer
        };

        let base = ModuleSpecifier::parse(base)
            .map_err(|e| io_err(format!("Invalid module referrer {}: {}", base, e)))?;
        self.import_map
            .try_borrow_mut()
            .map_err(|_| io_err("Import map is already borrowed".to_string()))?
            .resolve(specifier, &base)
            .map_err(io_err)
    }

    fn load(
        &self,
        module_specifier: &ModuleSpecifier,
        maybe_referrer: Option<&ModuleLoadReferrer>,
        options: ModuleLoadOptions,
    ) -> ModuleLoadResponse {
        let url = module_specifier.to_string();
        // Module-graph CORS and same-origin credentials are relative to the
        // owning document, not to the importing module. The importer remains
        // the HTTP referrer for a dependency; keeping these URLs distinct
        // prevents a cross-origin parent module from gaining CDN cookies when
        // it imports a sibling on that CDN.
        let document_url = ModuleSpecifier::parse(&self.base_url)
            .unwrap_or_else(|_| module_specifier.clone());
        let referrer = maybe_referrer
            .map(|referrer| referrer.specifier.clone())
            .unwrap_or_else(|| document_url.clone());
        // Capture the loader's proxy here so the async closure below owns a
        // plain Option<String> rather than borrowing &self across an `await`.
        let proxy_url = self.proxy_url.clone();
        let activity = self.activity.clone();
        let loaded_specifiers = self.loaded_specifiers.clone();
        loaded_specifiers.borrow_mut().push(url.clone());
        // Register before returning the future. The lifecycle can inspect the
        // runtime between deno_core accepting the load and first polling it.
        // Keeping the guard inside the future makes cancellation/navigation
        // decrement the count through Drop as well as success and failure.
        let is_dyn_import = options.is_dynamic_import;
        let javascript_requested = matches!(options.requested_module_type, deno_core::RequestedModuleType::None);
        let activity_guard = is_dyn_import.then(|| activity.begin());
        let page_state = self.page_state.clone();
        let page_network = match self.page_state.as_ref() {
            Some(weak) => (|| {
                let state = weak
                    .upgrade()
                    .ok_or_else(|| "Module loader page state was dropped".to_string())?;
                let state = state
                    .try_borrow()
                    .map_err(|_| "Module loader page state is already borrowed".to_string())?;
                let client = state
                    .http_client
                    .clone()
                    .ok_or_else(|| "No http_client wired to module loader".to_string())?;
                // Fork: in stealth mode an ES module must be fetched over the
                // same transport as the document. Upstream sends it through the
                // plain reqwest client, so a `type="module"` script arrives with
                // a different TLS fingerprint and none of the browser identity
                // headers, while the HTML that referenced it came over wreq.
                // That cross-transport mismatch is trivially detectable.
                #[cfg(feature = "stealth")]
                let stealth = state.stealth_client.clone();
                #[cfg(not(feature = "stealth"))]
                let stealth: Option<std::sync::Arc<()>> = None;
                let interception = if state.intercept_enabled {
                    state.intercept_tx.clone().map(|tx| (tx, state.intercept_page_id.clone(), state.intercept_url_patterns.clone(), state.cdp_request_patterns.clone()))
                } else { None };
                Ok((client, stealth, state.callbacks.clone(), interception))
            })(),
            None => self
                .standalone_client
                .clone()
                .map(|client| {
                    #[cfg(feature = "stealth")]
                    let stealth = None;
                    #[cfg(not(feature = "stealth"))]
                    let stealth: Option<std::sync::Arc<()>> = None;
                    (client, stealth, None, None)
                })
                .ok_or_else(|| "No network context wired to module loader".to_string()),
        };

        ModuleLoadResponse::Async(Pin::from(Box::new(async move {
            // deno_core propagates `is_dynamic_import` to every dependency edge in
            // the recursive graph, so this excludes parser-discovered/static
            // graphs without losing descendant fetches of a lazy graph.
            let _activity_guard = activity_guard;
            tracing::debug!(
                "Loading ES module: {} (proxy: {})",
                url,
                proxy_url.as_deref().unwrap_or("direct")
            );

            if url.starts_with("data:") || url.starts_with("blob:") {
                if !javascript_requested { return Err(io_err("Unsupported local module type".into())); }
                let requested = ModuleSpecifier::parse(&url).map_err(|e| io_err(e.to_string()))?;
                let body = local_module_source(&url, page_state.as_ref()).map_err(io_err)?;
                let code = String::from_utf8_lossy(&body).into_owned();
                return Ok(ModuleSource::new_with_redirect(
                    deno_core::ModuleType::JavaScript, ModuleSourceCode::String(code.into()),
                    &requested, &requested, None,
                ));
            }

            match page_network {
                Ok((client, stealth, callbacks, interception)) => {
                    let requested = ModuleSpecifier::parse(&url)
                        .map_err(|e| io_err(format!("Invalid module URL {}: {}", url, e)))?;
                    static NEXT_MODULE_REQUEST_ID: std::sync::atomic::AtomicU64 =
                        std::sync::atomic::AtomicU64::new(1);
                    let request_id = format!("module-{}", NEXT_MODULE_REQUEST_ID.fetch_add(
                        1, std::sync::atomic::Ordering::Relaxed,
                    ));
                    // Keep failed and cancelled graph fetches visible to the
                    // same Network listeners as ordinary fetch/XHR failures.
                    let mut network_guard = page_state.as_ref().map(|state| FetchNetworkGuard {
                        state: state.clone(), cancel: None,
                        event: Some(JsNetworkEvent {
                            request_id: request_id.clone(), intercepted: false,
                            url: url.clone(), method: "GET".to_string(),
                            request_headers: Default::default(),
                            resource_type: "Script".to_string(), status: 0,
                            response_headers: Default::default(), body_size: 0,
                            timestamp: 0.0, error_text: None,
                        }),
                    });
                    let request =
                        obscura_net::ResourceRequest::module_script(&document_url, &referrer);
                    let mut target = requested.clone();
                    let mut intercepted = false;
                    let mut synthetic = None;
                    let mut changed_method = None;
                    let mut changed_headers = None;
                    let mut changed_body = None;
                    if let Some((tx, page_id, patterns, cdp_patterns)) = interception {
                        if crate::ops::should_intercept_url(true, &patterns, &url)
                            && crate::ops::matches_cdp_request_patterns(cdp_patterns.as_deref().map(Vec::as_slice), &url, "Script") {
                            crate::ops::validate_fetch_url(&requested, client.allow_private_network).map_err(io_err)?;
                            let (resolver, resolution) = tokio::sync::oneshot::channel();
                            if tx.send(crate::ops::InterceptedRequest { request_id: request_id.clone(), page_id,
                                url: url.clone(), method: "GET".into(), headers: Default::default(),
                                resource_type: "Script".into(), resolver }).is_ok() {
                                intercepted = true;
                                if let Some(event) = network_guard.as_mut().and_then(|guard| guard.event.as_mut()) {
                                    event.intercepted = true;
                                }
                                match resolution.await.map_err(|_| io_err("module interception cancelled".into()))? {
                                    crate::ops::InterceptResolution::Continue { url, method, headers, body } => {
                                        if let Some(url) = url {
                                            target = ModuleSpecifier::parse(&url).map_err(|e| io_err(e.to_string()))?;
                                            crate::ops::validate_fetch_url(&target, client.allow_private_network).map_err(io_err)?;
                                        }
                                        changed_method = method; changed_headers = headers; changed_body = body;
                                    }
                                    crate::ops::InterceptResolution::Fulfill { status, headers, body, body_base64 } => {
                                        obscura_net::client::validate_intercepted_response(&request, &target, &headers)
                                            .map_err(|e| io_err(e.to_string()))?;
                                        let headers = obscura_net::client::normalize_intercepted_headers(headers);
                                        let body = if body_base64.is_empty() { body.into_bytes() } else {
                                            BASE64.decode(body_base64).map_err(|e| io_err(e.to_string()))?
                                        };
                                        if body.len() > request.max_response_bytes { return Err(io_err("fulfilled module too large".into())); }
                                        synthetic = Some(obscura_net::Response { url: target.clone(), status, headers, body, redirected_from: Vec::new() });
                                    }
                                    crate::ops::InterceptResolution::Fail { reason } => {
                                        if let Some(guard) = &mut network_guard { guard.event = None; }
                                        return Err(io_err(reason));
                                    }
                                }
                            }
                        }
                    }
                    #[cfg(feature = "stealth")]
                    let resp = if let Some(response) = synthetic { response } else { match stealth {
                        Some(stealth) if intercepted => stealth.fetch_resource_request(&target, request,
                            changed_method.as_deref().unwrap_or("GET"), changed_body, changed_headers, callbacks.as_deref()).await,
                        Some(stealth) => stealth
                            .fetch_resource_with_callbacks(
                                &requested,
                                request,
                                callbacks.as_deref(),
                            )
                            .await,
                        None if intercepted => client.fetch_resource_request(&target, request,
                            changed_method.as_deref().unwrap_or("GET"), changed_body, changed_headers, callbacks.as_deref()).await,
                        None => {
                            client
                                .fetch_resource_with_callbacks(
                                    &requested,
                                    request,
                                    callbacks.as_deref(),
                                )
                                .await
                        }
                    }
                    .map_err(|e| io_err(format!("Failed to fetch module {}: {}", url, e)))? };
                    #[cfg(not(feature = "stealth"))]
                    let resp = if let Some(response) = synthetic { response } else {
                        let _ = &stealth;
                        if intercepted { client.fetch_resource_request(&target, request, changed_method.as_deref().unwrap_or("GET"),
                            changed_body, changed_headers, callbacks.as_deref()).await }
                        else { client
                            .fetch_resource_with_callbacks(
                                &requested,
                                request,
                                callbacks.as_deref(),
                            )
                            .await }
                            .map_err(|e| io_err(format!("Failed to fetch module {}: {}", url, e)))?
                    };
                    // A response (including the blocked-tracker sentinel) is
                    // not a transport failure, even if evaluation later fails.
                    if let Some(guard) = &mut network_guard { guard.event = None; }
                    // A module response belongs to the owning page regardless
                    // of whether graph preparation or evaluation later fails.
                    // Status 0 is the transport's blocked-tracker sentinel.
                    if resp.status != 0 {
                        if let Some(state) = page_state.as_ref().and_then(Weak::upgrade) {
                            let mut state = state.try_borrow_mut().map_err(|_| {
                                io_err("Module loader page state is already borrowed".to_string())
                            })?;
                            record_network_completion(&mut state, JsNetworkEvent {
                                request_id, intercepted,
                                url: resp.url.to_string(), method: "GET".to_string(),
                                request_headers: Default::default(),
                                resource_type: "Script".to_string(), status: resp.status,
                                response_headers: resp.headers.clone(), body_size: resp.body.len(),
                                timestamp: std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default().as_secs_f64(),
                                error_text: None,
                            }, || match std::str::from_utf8(&resp.body) {
                                Ok(body) => StoredNetworkResponseBody {
                                    body: body.to_string(), base64_encoded: false,
                                },
                                Err(_) => StoredNetworkResponseBody {
                                    body: BASE64.encode(&resp.body), base64_encoded: true,
                                },
                            });
                        }
                    }
                    if !(200..=299).contains(&resp.status) {
                        return Err(io_err(format!(
                            "Module {} returned HTTP {}",
                            url, resp.status
                        )));
                    }
                    let found = ModuleSpecifier::parse(resp.url.as_str()).map_err(|e| {
                        io_err(format!("Invalid final module URL {}: {}", resp.url, e))
                    })?;
                    if found.as_str() != requested.as_str() {
                        loaded_specifiers
                            .borrow_mut()
                            .push(found.to_string());
                    }
                    let code = obscura_net::decode_non_html(&resp.body, resp.content_type());
                    Ok(ModuleSource::new_with_redirect(
                        deno_core::ModuleType::JavaScript,
                        ModuleSourceCode::String(code.into()),
                        &requested,
                        &found,
                        None,
                    ))
                }
                Err(error) => Err(io_err(error)),
            }
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_modules_decode_and_require_javascript_mime() {
        assert_eq!(local_module_source("data:text/javascript,export%20default%2042", None).unwrap(), b"export default 42");
        assert_eq!(local_module_source("data:application/javascript;base64,ZXhwb3J0IGRlZmF1bHQgNDM=", None).unwrap(), b"export default 43");
        assert!(local_module_source("data:text/javascript1.5,export%20default%2044", None).is_ok());
        assert!(local_module_source("data:text/plain,export%20default%2045", None).is_err());
        assert!(local_module_source("data:,export%20default%2046", None).is_err());
        assert!(local_module_source("data:text/javascript;base64,%%%", None).is_err());
    }
}
