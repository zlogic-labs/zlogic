use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::stream::BoxStream;
use zlogic_protocol::config::NetworkConfig;
use zlogic_protocol::llm::{LlmError, LlmErrorKind};
use zlogic_protocol::settings::NetworkSettings;

pub type ByteStream = BoxStream<'static, Result<Bytes, LlmError>>;

#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub network: NetworkConfig,
}

impl HttpRequest {
    pub fn json(url: impl Into<String>, body: Vec<u8>) -> Self {
        Self {
            url: url.into(),
            headers: vec![("content-type".into(), "application/json".into())],
            body,
            network: NetworkConfig::default(),
        }
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn connect_timeout(&self) -> Option<Duration> {
        self.network.connect_timeout_ms.map(Duration::from_millis)
    }

    pub fn read_timeout(&self) -> Option<Duration> {
        self.network.read_timeout_ms.map(Duration::from_millis)
    }
}

#[async_trait]
pub trait HttpTransport: Send + Sync {
    async fn post_stream(&self, req: HttpRequest) -> Result<ByteStream, LlmError>;

    fn apply_network(&self, _network: &NetworkSettings) {}

    async fn get(&self, url: &str) -> Result<Vec<u8>, LlmError> {
        Err(LlmError {
            kind: LlmErrorKind::Network,
            message: format!("this transport does not support GET ({url})"),
            retryable: false,
            status: None,
            request_id: None,
        })
    }
}

#[cfg(feature = "http")]
mod reqwest_impl {
    use super::*;
    use crate::error;
    use futures_util::{StreamExt, stream};

    const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

    pub struct ReqwestTransport {
        client: std::sync::RwLock<Arc<reqwest::Client>>,
        network: Mutex<NetworkSettings>,
    }

    impl ReqwestTransport {
        pub fn new(client: reqwest::Client) -> Self {
            Self {
                client: std::sync::RwLock::new(Arc::new(client)),
                network: Mutex::new(NetworkSettings::default()),
            }
        }

        pub fn with_network(network: &NetworkSettings) -> Self {
            Self {
                client: std::sync::RwLock::new(Arc::new(build_client(network))),
                network: Mutex::new(network.clone()),
            }
        }

        fn client(&self) -> Arc<reqwest::Client> {
            self.client.read().unwrap().clone()
        }
    }

    impl Default for ReqwestTransport {
        fn default() -> Self {
            Self::with_network(&NetworkSettings::default())
        }
    }

    fn build_client(network: &NetworkSettings) -> reqwest::Client {
        let mut builder = reqwest::Client::builder();
        if let Some(url) = network.proxy_url() {
            match reqwest::Proxy::all(url) {
                Ok(proxy) => {
                    let no_proxy = network.no_proxy_list();
                    builder = builder.proxy(if no_proxy.is_empty() {
                        proxy
                    } else {
                        proxy.no_proxy(Some(
                            reqwest::NoProxy::from_string(&no_proxy).unwrap_or_default(),
                        ))
                    });
                }
                Err(error) => {
                    tracing::warn!(
                        target: "zlogic::llm",
                        proxy = url,
                        "ignoring a proxy URL reqwest cannot use: {error}"
                    );
                }
            }
        }
        builder.build().unwrap_or_else(|error| {
            tracing::warn!(
                target: "zlogic::llm",
                "could not build the HTTP client ({error}); falling back to the default one"
            );
            reqwest::Client::new()
        })
    }

    fn with_read_timeout(
        inner: ByteStream,
        timeout: Duration,
    ) -> impl futures_core::Stream<Item = Result<Bytes, LlmError>> {
        stream::unfold((inner, timeout), |(mut inner, timeout)| async move {
            match tokio::time::timeout(timeout, inner.next()).await {
                Ok(Some(item)) => Some((item, (inner, timeout))),
                Ok(None) => None,
                Err(_) => Some((
                    Err(LlmError {
                        kind: LlmErrorKind::Network,
                        retryable: false,
                        message: format!(
                            "read timeout: no data from the provider within {timeout:?}"
                        ),
                        status: None,
                        request_id: None,
                    }),
                    (futures_util::stream::empty().boxed(), timeout),
                )),
            }
        })
    }

    #[async_trait]
    impl HttpTransport for ReqwestTransport {
        fn apply_network(&self, network: &NetworkSettings) {
            {
                let current = self.network.lock().unwrap();
                if *current == *network {
                    return;
                }
            }
            *self.network.lock().unwrap() = network.clone();
            *self.client.write().unwrap() = Arc::new(build_client(network));
            tracing::info!(target: "zlogic::llm", "outbound proxy settings updated");
        }

        async fn get(&self, url: &str) -> Result<Vec<u8>, LlmError> {
            let client = self.client();
            let fetch = async {
                let resp = client
                    .get(url)
                    .send()
                    .await
                    .map_err(|e| error::network(e.to_string()))?;
                let status = resp.status().as_u16();
                if !(200..300).contains(&status) {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(error::from_status(
                        status,
                        &body,
                        &error::ResponseMeta::default(),
                    ));
                }
                resp.bytes()
                    .await
                    .map(|b| b.to_vec())
                    .map_err(|e| error::network(e.to_string()))
            };
            tokio::time::timeout(Duration::from_secs(60), fetch)
                .await
                .map_err(|_| error::network(format!("GET {url} timed out after 60s")))?
        }

        async fn post_stream(&self, req: HttpRequest) -> Result<ByteStream, LlmError> {
            let connect = req.connect_timeout().unwrap_or(DEFAULT_CONNECT_TIMEOUT);
            let read = req.read_timeout();

            let client = self.client();
            let mut builder = client.post(&req.url).body(req.body);
            for (k, v) in &req.headers {
                builder = builder.header(k.as_str(), v.as_str());
            }

            let resp = tokio::time::timeout(connect, builder.send())
                .await
                .map_err(|_| {
                    error::network(format!(
                        "connect timeout: no response headers from {} within {connect:?}",
                        req.url
                    ))
                })?
                .map_err(|e| {
                    if e.is_timeout() {
                        error::network(format!("timeout: {e}"))
                    } else {
                        error::network(e.to_string())
                    }
                })?;

            let status = resp.status().as_u16();
            if !(200..300).contains(&status) {
                let pairs: Vec<(String, String)> = resp
                    .headers()
                    .iter()
                    .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
                    .collect();
                let meta = error::ResponseMeta::from_headers(
                    pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())),
                );
                let body = resp.text().await.unwrap_or_default();
                return Err(error::from_status(status, &body, &meta));
            }

            let stream = resp
                .bytes_stream()
                .map(|r| r.map_err(|e| error::protocol(format!("stream error: {e}"))));
            let stream = match read {
                Some(d) => with_read_timeout(stream.boxed(), d).boxed(),
                None => stream.boxed(),
            };
            Ok(Box::pin(stream))
        }
    }
}

#[cfg(feature = "http")]
pub use reqwest_impl::ReqwestTransport;

pub struct ReplayTransport {
    chunks: Vec<Bytes>,
}

impl ReplayTransport {
    pub fn whole(body: impl Into<Vec<u8>>) -> Self {
        Self {
            chunks: vec![Bytes::from(body.into())],
        }
    }

    pub fn chunked(body: impl AsRef<[u8]>, size: usize) -> Self {
        let body = body.as_ref();
        let size = size.max(1);
        Self {
            chunks: body.chunks(size).map(Bytes::copy_from_slice).collect(),
        }
    }

    pub fn from_chunks(chunks: Vec<Vec<u8>>) -> Self {
        Self {
            chunks: chunks.into_iter().map(Bytes::from).collect(),
        }
    }
}

#[async_trait]
impl HttpTransport for ReplayTransport {
    async fn post_stream(&self, _req: HttpRequest) -> Result<ByteStream, LlmError> {
        let chunks = self.chunks.clone();
        Ok(Box::pin(futures_util::stream::iter(
            chunks.into_iter().map(Ok),
        )))
    }
}

#[derive(Default, Clone)]
pub struct RecordingTransport {
    last: Arc<Mutex<Option<HttpRequest>>>,
}

impl RecordingTransport {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn last(&self) -> Option<HttpRequest> {
        self.last.lock().ok()?.clone()
    }

    pub fn last_body(&self) -> Option<String> {
        self.last()
            .map(|r| String::from_utf8_lossy(&r.body).into_owned())
    }
}

#[async_trait]
impl HttpTransport for RecordingTransport {
    async fn post_stream(&self, req: HttpRequest) -> Result<ByteStream, LlmError> {
        if let Ok(mut slot) = self.last.lock() {
            *slot = Some(req);
        }
        Ok(Box::pin(futures_util::stream::empty()))
    }
}

#[derive(Default)]
pub struct FailingTransport {
    pub status: u16,
    pub body: String,
    pub headers: Vec<(String, String)>,
}

impl FailingTransport {
    pub fn new(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
            headers: Vec::new(),
        }
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

#[async_trait]
impl HttpTransport for FailingTransport {
    async fn post_stream(&self, _req: HttpRequest) -> Result<ByteStream, LlmError> {
        let meta = crate::error::ResponseMeta::from_headers(
            self.headers.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        );
        Err(crate::error::from_status(self.status, &self.body, &meta))
    }
}

pub struct TruncatingTransport {
    pub prefix: Vec<u8>,
}

#[async_trait]
impl HttpTransport for TruncatingTransport {
    async fn post_stream(&self, _req: HttpRequest) -> Result<ByteStream, LlmError> {
        let prefix = Bytes::from(self.prefix.clone());
        let items: Vec<Result<Bytes, LlmError>> =
            vec![Ok(prefix), Err(crate::error::protocol("connection reset"))];
        Ok(Box::pin(futures_util::stream::iter(items)))
    }
}
