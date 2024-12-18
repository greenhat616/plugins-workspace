use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use futures_util::SinkExt;
use futures_util::StreamExt;
use http::HeaderName;
use http::HeaderValue;
use http_body_util::BodyExt;
use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_tungstenite::{HyperWebsocket, WebSocketStream};
use hyper_util::rt::TokioIo;
use tauri::AssetResolver;
use tauri::{
    plugin::{Builder as PluginBuilder, TauriPlugin},
    Runtime,
};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tungstenite::protocol::Message;

type Error = Box<dyn std::error::Error + Send + Sync + 'static>;

type BoxBody = http_body_util::combinators::BoxBody<Bytes, Infallible>;

pub struct LocalRequest {
    url: String,
    headers: HashMap<String, String>,
}

impl LocalRequest {
    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn headers(&self) -> &HashMap<String, String> {
        &self.headers
    }
}

pub struct LocalResponse {
    headers: HashMap<String, String>,
}

impl LocalResponse {
    pub fn add_header<H: Into<String>, V: Into<String>>(&mut self, header: H, value: V) {
        self.headers.insert(header.into(), value.into());
    }
}

pub struct Builder {
    port: u16,
    host: Option<String>,
}

impl Builder {
    pub fn new(port: u16) -> Self {
        Self { port, host: None }
    }

    pub fn host<H: Into<String>>(mut self, host: H) -> Self {
        self.host = Some(host.into());
        self
    }

    pub fn build<R: Runtime>(self) -> TauriPlugin<R> {
        let port = self.port;
        let host = self.host.unwrap_or_else(|| "127.0.0.1".to_string());

        PluginBuilder::new("localhost")
            .setup(move |app, _api| {
                let asset_resolver = app.asset_resolver();
                let dev_url = app.config().build.dev_url.clone();
                let is_dev = tauri::is_dev();

                let asset_resolver = Arc::new(RwLock::new(asset_resolver));

                let server = async move {
                    let addr: SocketAddr = format!("{}:{}", host, port).parse().unwrap();
                    log::info!("Listening on http://{}", addr);

                    let listener = TcpListener::bind(addr).await.unwrap();

                    let handle_request_handler = move |req: Request<Incoming>| {
                        let asset_resolver = asset_resolver.clone();
                        let dev_url = dev_url.clone();

                        async move {
                            if hyper_tungstenite::is_upgrade_request(&req) {
                                let path = req.uri().path().to_string();
                                let (response, websocket) = hyper_tungstenite::upgrade(req, None)?;

                                tokio::spawn(async move {
                                    // pipe to devUrl websocket
                                    // assert dev_url is Some
                                    let dev_url = dev_url.clone().unwrap();
                                    let mut proxy_url = dev_url.join(&path).unwrap();
                                    proxy_url.set_scheme("ws").unwrap();
                                    let handle_ws = move |ws: HyperWebsocket| async move {
                                        let websocket = ws.await?;
                                        let (mut server_write, mut server_read) = websocket.split();
                                        // connect to dev server
                                        let (socket, _client_response) =
                                            tokio_tungstenite::connect_async(proxy_url.as_str())
                                                .await?;
                                        let (mut client_write, mut client_read) = socket.split();
                                        tokio::spawn(async move {
                                            while let Some(Ok(message)) = client_read.next().await {
                                                if let Err(e) = server_write.send(message).await {
                                                    log::error!(
                                                        "Error sending message to server: {e}"
                                                    );
                                                }
                                            }
                                        });
                                        while let Some(Ok(message)) = server_read.next().await {
                                            if let Err(e) = client_write.send(message).await {
                                                log::error!("Error sending message to client: {e}");
                                            }
                                        }
                                        Ok::<(), Error>(())
                                    };
                                    if let Err(e) = handle_ws(websocket).await {
                                        eprintln!("Error in websocket connection: {e}");
                                    }
                                });

                                return Ok::<_, Error>(response);
                            }
                            let path = req.uri().path().to_string();
                            let resolver = asset_resolver.read().await;

                            if let Some(asset) = resolver.get(path.clone()) {
                                let mut local_response = LocalResponse {
                                    headers: Default::default(),
                                };

                                local_response.add_header("Content-Type", &asset.mime_type);
                                if let Some(csp) = asset.csp_header {
                                    local_response.add_header("Content-Security-Policy", &csp);
                                }

                                let mut response = Response::builder();
                                for (name, value) in local_response.headers {
                                    if let Ok(header_name) = name.parse::<HeaderName>() {
                                        if let Ok(header_value) = value.parse::<HeaderValue>() {
                                            response = response.header(header_name, header_value);
                                        }
                                    }
                                }
                                let response = response.body(Full::from(asset.bytes))?;
                                Ok(response)
                            } else if is_dev && dev_url.is_some() {
                                // Proxy to dev server
                                let client = reqwest::Client::new();
                                let dev_url = dev_url.clone().unwrap();
                                let url = dev_url.join(&path).unwrap();

                                let mut proxy_req = client.request(req.method().clone(), url);

                                // Copy headers
                                for (name, value) in req.headers() {
                                    proxy_req = proxy_req.header(name, value);
                                }

                                match proxy_req.send().await {
                                    Ok(proxy_res) => {
                                        let mut response =
                                            Response::builder().status(proxy_res.status());

                                        // Copy response headers
                                        for (name, value) in proxy_res.headers() {
                                            response = response.header(name, value);
                                        }

                                        let body = proxy_res.bytes().await.unwrap_or_default();
                                        let response = response.body(Full::from(body))?;
                                        Ok(response)
                                    }
                                    Err(_) => Ok(Response::builder()
                                        .status(hyper::StatusCode::BAD_GATEWAY)
                                        .body(Full::default())?),
                                }
                            } else {
                                Ok(Response::builder()
                                    .status(hyper::StatusCode::NOT_FOUND)
                                    .header("Content-Type", "text/html")
                                    .header("Content-Security-Policy", "default-src 'none'")
                                    .body(Full::default())?)
                            }
                        }
                    };

                    loop {
                        if let Ok((stream, _)) = listener.accept().await {
                            let mut http = hyper::server::conn::http1::Builder::new();
                            http.keep_alive(true);
                            let connection = http
                                .serve_connection(
                                    TokioIo::new(stream),
                                    service_fn(handle_request_handler.clone()),
                                )
                                .with_upgrades();
                            tokio::spawn(connection);
                        }
                    }
                };
                let handle = tokio::runtime::Handle::try_current();
                match handle {
                    Ok(handle) => {
                        handle.spawn(server);
                    }
                    Err(_) => {
                        std::thread::spawn(move || {
                            let rt = tokio::runtime::Builder::new_multi_thread()
                                .enable_all()
                                .build()
                                .unwrap();
                            rt.block_on(server);
                        });
                    }
                }

                Ok(())
            })
            .build()
    }
}
