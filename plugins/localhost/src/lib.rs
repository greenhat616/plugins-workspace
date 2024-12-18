// Copyright 2019-2023 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

//! Expose your apps assets through a localhost server instead of the default custom protocol.
//!
//! **Note: This plugins brings considerable security risks and you should only use it if you know what your are doing. If in doubt, use the default custom protocol implementation.**

#![doc(
    html_logo_url = "https://github.com/tauri-apps/tauri/raw/dev/app-icon.png",
    html_favicon_url = "https://github.com/tauri-apps/tauri/raw/dev/app-icon.png"
)]

use std::collections::HashMap;

use http::Uri;
use tauri::{
    plugin::{Builder as PluginBuilder, TauriPlugin},
    Runtime,
};
use tiny_http::{Header, Response as HttpResponse, Server};

pub struct Request {
    url: String,
}

impl Request {
    pub fn url(&self) -> &str {
        &self.url
    }
}

pub struct Response {
    headers: HashMap<String, String>,
}

impl Response {
    pub fn add_header<H: Into<String>, V: Into<String>>(&mut self, header: H, value: V) {
        self.headers.insert(header.into(), value.into());
    }
}

type OnRequest = Option<Box<dyn Fn(&Request, &mut Response) + Send + Sync>>;

pub struct Builder {
    port: u16,
    host: Option<String>,
    on_request: OnRequest,
}

impl Builder {
    pub fn new(port: u16) -> Self {
        Self {
            port,
            host: None,
            on_request: None,
        }
    }

    // Change the host the plugin binds to. Defaults to `localhost`.
    pub fn host<H: Into<String>>(mut self, host: H) -> Self {
        self.host = Some(host.into());
        self
    }

    pub fn on_request<F: Fn(&Request, &mut Response) + Send + Sync + 'static>(
        mut self,
        f: F,
    ) -> Self {
        self.on_request.replace(Box::new(f));
        self
    }

    pub fn build<R: Runtime>(mut self) -> TauriPlugin<R> {
        let port = self.port;
        let host = self.host.unwrap_or("localhost".to_string());
        let on_request = self.on_request.take();

        PluginBuilder::new("localhost")
            .setup(move |app, _api| {
                let asset_resolver = app.asset_resolver();
                let dev_url = app.config().build.dev_url.clone();
                let is_dev = tauri::is_dev();
                std::thread::spawn(move || {
                    let server =
                        Server::http(format!("{host}:{port}")).expect("Unable to spawn server");
                    for req in server.incoming_requests() {
                        let path: String = req
                            .url()
                            .parse::<Uri>()
                            .map(|uri| uri.path().into())
                            .unwrap_or_else(|_| req.url().into());
                        println!("path: {}", path);
                        #[allow(unused_mut)]
                        if let Some(mut asset) = asset_resolver.get(path.clone()) {
                            let request = Request {
                                url: req.url().into(),
                            };
                            let mut response = Response {
                                headers: Default::default(),
                            };

                            response.add_header("Content-Type", asset.mime_type);
                            if let Some(csp) = asset.csp_header {
                                response
                                    .headers
                                    .insert("Content-Security-Policy".into(), csp);
                            }

                            if let Some(on_request) = &on_request {
                                on_request(&request, &mut response);
                            }

                            let mut resp = HttpResponse::from_data(asset.bytes);
                            for (header, value) in response.headers {
                                if let Ok(h) = Header::from_bytes(header.as_bytes(), value) {
                                    resp.add_header(h);
                                }
                            }
                            req.respond(resp).expect("unable to setup response");
                        } else {
                            if is_dev && dev_url.is_some() {
                                // try to pipe the request path to the dev server
                                let dev_url = dev_url.as_ref().unwrap();
                                let url = dev_url.join(&path).unwrap();
                                log::debug!("fetching dev server asset: {}", url);
                                println!("fetching dev server asset: {}", url);
                                match ureq::get(url.as_str()).call() {
                                    Ok(response) => {
                                        let headers = response.headers_names();
                                        let headers = headers
                                            .into_iter()
                                            .map(|header| {
                                                let value =
                                                    response.header(&header).unwrap().to_string();
                                                (header, value)
                                            })
                                            .collect::<HashMap<_, _>>();
                                        let content_len =
                                            response.header("Content-Length").unwrap_or("1024");
                                        let content_len =
                                            content_len.parse::<usize>().unwrap();
                                        let mut buffer = vec![0; content_len];
                                        response.into_reader().read_to_end(&mut buffer).unwrap();
                                        buffer.shrink_to_fit();
                                        let mut resp = HttpResponse::from_data(buffer);
                                        for (header, value) in headers {
                                            if let Ok(h) =
                                                Header::from_bytes(header.as_bytes(), value)
                                            {
                                                resp.add_header(h);
                                            }
                                        }
                                        req.respond(resp).expect("unable to setup response");
                                        continue;
                                    }
                                    Err(e) => {
                                        log::error!("failed to fetch dev server asset: {}", e);
                                    }
                                }
                            }

                            log::debug!("asset not found");
                            let mut resp = HttpResponse::empty(404);
                            resp.add_header(
                                Header::from_bytes("Content-Type", "text/html").unwrap(),
                            );
                            resp.add_header(
                                Header::from_bytes("Content-Security-Policy", "default-src 'none'")
                                    .unwrap(),
                            );
                            req.respond(resp).expect("unable to setup response");
                        }
                    }
                });
                Ok(())
            })
            .build()
    }
}
