use core::pin::Pin;
use futures::channel::mpsc;
use futures::channel::oneshot;
use futures::future::FutureExt;
use futures::stream::{Stream, StreamExt, TryStreamExt};
use http::{Method, Request, Response};
use hyper::body::Bytes;
use hyper::Body;
use serde_urlencoded;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use url::Url;

use crate::dynamic_resources;
use crate::util::{
    finish_detectable_stream, one_stream, FinishDetectableStream, HeaderValuesBuilder,
    OptionHeaderBuilder,
};

pub mod reserved_paths {
    crate::with_values! {
        pub const INDEX: &'static str = "/";
        pub const NO_SCRIPT: &'static str = "/noscript";
        pub const VERSION: &'static str = "/version";
        pub const HELP: &'static str = "/help";
        pub const FAVICON_ICO: &'static str = "/favicon.ico";
        pub const ROBOTS_TXT: &'static str = "/robots.txt";
    }
}

pub const NO_SCRIPT_PATH_QUERY_PARAMETER_NAME: &str = "path";

struct DataSender {
    req: Request<Body>,
    res_body_streams_sender: RwLock<
        mpsc::UnboundedSender<
            Pin<Box<dyn Stream<Item = Result<Bytes, std::convert::Infallible>> + Send>>,
        >,
    >,
}

struct DataReceiver {
    res_sender: oneshot::Sender<Response<Body>>,
}

pub struct PipingServer {
    path_to_sender: Arc<RwLock<HashMap<String, DataSender>>>,
    path_to_receiver: Arc<RwLock<HashMap<String, DataReceiver>>>,
}

impl Clone for PipingServer {
    fn clone(&self) -> Self {
        PipingServer {
            path_to_sender: Arc::clone(&self.path_to_sender),
            path_to_receiver: Arc::clone(&self.path_to_receiver),
        }
    }
}

impl PipingServer {
    pub fn new() -> Self {
        PipingServer {
            path_to_sender: Arc::new(RwLock::new(HashMap::new())),
            path_to_receiver: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn handler(
        &self,
        uses_https: bool,
        req: Request<Body>,
        res_sender: oneshot::Sender<Response<Body>>,
    ) -> impl std::future::Future<Output = ()> {
        let path_to_sender = Arc::clone(&self.path_to_sender);
        let path_to_receiver = Arc::clone(&self.path_to_receiver);
        async move {
            let path = req.uri().path();

            log::info!("{} {:} {:?}", req.method(), req.uri(), req.version());

            if req.method() == Method::GET || req.method() == Method::HEAD {
                match path {
                    reserved_paths::INDEX => {
                        let res = Response::builder()
                            .status(200)
                            .header("Content-Type", "text/html")
                            .header("Access-Control-Allow-Origin", "*")
                            .body(Body::from(dynamic_resources::index()))
                            .unwrap();
                        res_sender.send(res).unwrap();
                        return;
                    }
                    reserved_paths::NO_SCRIPT => {
                        let query_params = match req.uri().query() {
                            Some(query) => {
                                serde_urlencoded::from_str::<HashMap<String, String>>(query)
                                    .unwrap_or_else(|_| HashMap::new())
                            }
                            None => HashMap::new(),
                        };
                        let html = dynamic_resources::no_script_html(&query_params);
                        let res = Response::builder()
                            .status(200)
                            .header("Content-Type", "text/html")
                            .header("Access-Control-Allow-Origin", "*")
                            .body(Body::from(html))
                            .unwrap();
                        res_sender.send(res).unwrap();
                        return;
                    }
                    reserved_paths::VERSION => {
                        let version: &'static str = env!("CARGO_PKG_VERSION");
                        let res = Response::builder()
                            .status(200)
                            .header("Content-Type", "text/plain")
                            .header("Access-Control-Allow-Origin", "*")
                            .body(Body::from(format!("{} (Rust)\n", version)))
                            .unwrap();
                        res_sender.send(res).unwrap();
                        return;
                    }
                    reserved_paths::HELP => {
                        let host: &str = req
                            .headers()
                            .get("host")
                            .map(|h| h.to_str().unwrap())
                            .unwrap_or_else(|| "hostname");
                        let x_forwarded_proto_is_https =
                            if let Some(proto) = req.headers().get("x-forwarded-proto") {
                                proto.to_str().unwrap().contains("https")
                            } else {
                                false
                            };
                        let schema = if uses_https || x_forwarded_proto_is_https {
                            "https"
                        } else {
                            "http"
                        };
                        let base_url = Url::parse(format!("{}://{}", schema, host).as_str())
                            .unwrap_or_else(|_| "http://hostname/".parse().unwrap());
                        let help = dynamic_resources::help(&base_url);
                        let res = Response::builder()
                            .status(200)
                            .header("Content-Type", "text/plain")
                            .header("Access-Control-Allow-Origin", "*")
                            .body(Body::from(help))
                            .unwrap();
                        res_sender.send(res).unwrap();
                        return;
                    }
                    reserved_paths::FAVICON_ICO => {
                        let res = Response::builder().status(204).body(Body::empty()).unwrap();
                        res_sender.send(res).unwrap();
                        return;
                    }
                    reserved_paths::ROBOTS_TXT => {
                        let res = Response::builder().status(404).body(Body::empty()).unwrap();
                        res_sender.send(res).unwrap();
                        return;
                    }
                    _ => {}
                }
            }

            match req.method() {
                &Method::GET => {
                    if let Some(value) = req.headers().get("service-worker") {
                        if value == http::HeaderValue::from_static("script") {
                            // Reject Service Worker registration
                            let res = Response::builder()
                                .status(400)
                                .header("Content-Type", "text/plain")
                                .header("Access-Control-Allow-Origin", "*")
                                .body(Body::from(
                                    "[ERROR] Service Worker registration is rejected.\n",
                                ))
                                .unwrap();
                            res_sender.send(res).unwrap();
                            return;
                        }
                    }
                    let receiver_connected: bool =
                        path_to_receiver.read().unwrap().contains_key(path);
                    // If a receiver has been connected already
                    if receiver_connected {
                        let res = Response::builder()
                            .status(400)
                            .header("Content-Type", "text/plain")
                            .header("Access-Control-Allow-Origin", "*")
                            .body(Body::from(format!(
                                "[ERROR] Another receiver has been connected on '{}'.\n",
                                path
                            )))
                            .unwrap();
                        res_sender.send(res).unwrap();
                        return;
                    }
                    let sender = path_to_sender.write().unwrap().remove(path);
                    match sender {
                        // If sender is found
                        Some(data_sender) => {
                            data_sender
                                .res_body_streams_sender
                                .write()
                                .unwrap()
                                .unbounded_send(
                                    one_stream(Ok(Bytes::from(
                                        "[INFO] A receiver was connected.\n",
                                    )))
                                    .boxed(),
                                )
                                .unwrap();
                            transfer(path.to_string(), data_sender, DataReceiver { res_sender })
                                .await
                                .unwrap();
                        }
                        // If sender is not found
                        None => {
                            path_to_receiver
                                .write()
                                .unwrap()
                                .insert(path.to_string(), DataReceiver { res_sender });
                        }
                    }
                }
                &Method::POST | &Method::PUT => {
                    if reserved_paths::VALUES.contains(&path) {
                        // Reject reserved path sending
                        let res = Response::builder()
                            .status(400)
                            .header("Content-Type", "text/plain")
                            .header("Access-Control-Allow-Origin", "*")
                            .body(Body::from(format!(
                                "[ERROR] Cannot send to the reserved path '{}'. (e.g. '/mypath123')\n",
                                path
                            )))
                            .unwrap();
                        res_sender.send(res).unwrap();
                        return;
                    }
                    // Notify that Content-Range is not supported
                    // In the future, resumable upload using Content-Range might be supported
                    // ref: https://github.com/httpwg/http-core/pull/653
                    if req.headers().contains_key("content-range") {
                        // Reject reserved path sending
                        let res = Response::builder()
                            .status(400)
                            .header("Content-Type", "text/plain")
                            .header("Access-Control-Allow-Origin", "*")
                            .body(Body::from(format!(
                                "[ERROR] Content-Range is not supported for now in {}\n",
                                req.method()
                            )))
                            .unwrap();
                        res_sender.send(res).unwrap();
                        return;
                    }
                    let sender_connected: bool = path_to_sender.read().unwrap().contains_key(path);
                    // If a sender has been connected already
                    if sender_connected {
                        let res = Response::builder()
                            .status(400)
                            .header("Content-Type", "text/plain")
                            .header("Access-Control-Allow-Origin", "*")
                            .body(Body::from(format!(
                                "[ERROR] Another sender has been connected on '{}'.\n",
                                path
                            )))
                            .unwrap();
                        res_sender.send(res).unwrap();
                        return;
                    }

                    let (tx, rx) = mpsc::unbounded::<
                        Pin<Box<dyn Stream<Item = Result<Bytes, std::convert::Infallible>> + Send>>,
                    >();
                    let body = hyper::body::Body::wrap_stream(rx.flatten());
                    let sender_res = Response::builder()
                        .header("Content-Type", "text/plain")
                        .header("Access-Control-Allow-Origin", "*")
                        .body(body)
                        .unwrap();
                    res_sender.send(sender_res).unwrap();

                    let receiver = path_to_receiver.write().unwrap().remove(path);
                    match receiver {
                        // If receiver is found
                        Some(data_receiver) => {
                            tx.unbounded_send(
                                one_stream(Ok(Bytes::from(
                                    "[INFO] 1 receiver(s) has/have been connected.\n",
                                )))
                                .boxed(),
                            )
                            .unwrap();
                            transfer(
                                path.to_string(),
                                DataSender {
                                    req,
                                    res_body_streams_sender: RwLock::new(tx),
                                },
                                data_receiver,
                            )
                            .await
                            .unwrap();
                        }
                        // If receiver is not found
                        None => {
                            tx.unbounded_send(
                                one_stream(Ok(Bytes::from(
                                    "[INFO] Waiting for 1 receiver(s)...\n",
                                )))
                                .boxed(),
                            )
                            .unwrap();
                            path_to_sender.write().unwrap().insert(
                                path.to_string(),
                                DataSender {
                                    req,
                                    res_body_streams_sender: RwLock::new(tx),
                                },
                            );
                        }
                    }
                }
                &Method::OPTIONS => {
                    // Response for Preflight request
                    let res = Response::builder()
                        .status(200)
                        .header("Access-Control-Allow-Origin", "*")
                        .header(
                            "Access-Control-Allow-Methods",
                            "GET, HEAD, POST, PUT, OPTIONS",
                        )
                        .header(
                            "Access-Control-Allow-Headers",
                            "Content-Type, Content-Disposition, X-Piping",
                        )
                        .header("Access-Control-Max-Age", 86400)
                        .header("Content-Length", 0)
                        .body(Body::empty())
                        .unwrap();
                    res_sender.send(res).unwrap();
                }
                _ => {
                    log::info!("Unsupported method: {}", req.method());
                    let res = Response::builder()
                        .status(405)
                        .header("Access-Control-Allow-Origin", "*")
                        .body(Body::from(format!(
                            "[ERROR] Unsupported method: {}.\n",
                            req.method()
                        )))
                        .unwrap();
                    res_sender.send(res).unwrap();
                }
            }
        }
    }
}

struct TransferRequest {
    content_type: Option<hyper::http::HeaderValue>,
    content_length: Option<hyper::http::HeaderValue>,
    content_disposition: Option<hyper::http::HeaderValue>,
    body: Body,
}

#[inline(always)]
fn raw_transfer_request(parts: &http::request::Parts, body: Body) -> TransferRequest {
    TransferRequest {
        content_type: parts.headers.get("content-type").cloned(),
        content_length: parts.headers.get("content-length").cloned(),
        content_disposition: parts.headers.get("content-disposition").cloned(),
        body,
    }
}

async fn get_transfer_request(
    parts: &http::request::Parts,
    body: Body,
) -> Result<TransferRequest, std::io::Error> {
    let content_type_option = parts.headers.get("content-type");
    if content_type_option.is_none() {
        return Ok(raw_transfer_request(parts, body));
    }
    let content_type = content_type_option.unwrap();
    let mime_type_result: Result<mime::Mime, _> = match content_type.to_str() {
        Ok(s) => s
            .parse()
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err)),
        Err(err) => Err(std::io::Error::new(std::io::ErrorKind::Other, err)),
    };
    if mime_type_result.is_err() {
        return Ok(raw_transfer_request(parts, body));
    }
    let mime_type = mime_type_result.unwrap();
    if mime_type.essence_str() != "multipart/form-data" {
        return Ok(raw_transfer_request(parts, body));
    }
    let boundary = mime_type
        .get_param("boundary")
        .map(|b| b.to_string())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "boundary not found"))?;
    let mut multipart_stream = mpart_async::server::MultipartStream::new(boundary, body);

    while let Ok(Some(field)) = multipart_stream.try_next().await {
        // NOTE: Only first one is transferred
        let headers = field.headers().clone();
        return Ok(TransferRequest {
            content_type: headers.get("content-type").cloned(),
            content_length: headers.get("content-length").cloned(),
            content_disposition: headers.get("content-disposition").cloned(),
            body: Body::wrap_stream(field),
        });
    }
    return Err(std::io::Error::new(
        std::io::ErrorKind::Other,
        "multipart error",
    ));
}

async fn transfer(
    path: String,
    data_sender: DataSender,
    data_receiver: DataReceiver,
) -> Result<(), std::io::Error> {
    let (data_sender_parts, data_sender_body) = data_sender.req.into_parts();
    log::info!("Transfer start: '{}'", path);
    // Extract transfer headers and body even when request is multipart
    let transfer_request = get_transfer_request(&data_sender_parts, data_sender_body).await?;
    // The finish_waiter will tell when the body is finished
    let (finish_detectable_body, sender_req_body_finish_waiter) =
        finish_detectable_stream(transfer_request.body);
    // Create receiver's body
    let receiver_res_body = Body::wrap_stream::<FinishDetectableStream<Body>, Bytes, hyper::Error>(
        finish_detectable_body,
    );
    let x_piping = data_sender_parts.headers.get_all("x-piping");
    let has_x_piping = data_sender_parts.headers.contains_key("x-piping");
    // Create receiver's response
    let receiver_res = Response::builder()
        .option_header("Content-Type", transfer_request.content_type)
        .option_header("Content-Length", transfer_request.content_length)
        .option_header("Content-Disposition", transfer_request.content_disposition)
        .header_values("X-Piping", x_piping.into_iter().cloned())
        .header("Access-Control-Allow-Origin", "*")
        .option_header(
            "Access-Control-Expose-Headers",
            if has_x_piping { Some("X-Piping") } else { None },
        )
        .header("X-Robots-Tag", "none")
        .body(receiver_res_body)
        .unwrap();
    // Return response to receiver
    data_receiver.res_sender.send(receiver_res).unwrap();

    data_sender
        .res_body_streams_sender
        .write()
        .unwrap()
        .unbounded_send(
            one_stream(Ok(Bytes::from(
                "[INFO] Start sending to 1 receiver(s)...\n",
            )))
            .chain(
                // Wait for sender's request body finished
                sender_req_body_finish_waiter
                    .into_stream()
                    .map(|_| Ok(Bytes::new())),
            )
            .chain(
                // Notify sender when sending finished
                one_stream(Ok(Bytes::from("[INFO] Sent successfully!\n"))),
            )
            .chain(one_stream(Ok(Bytes::new())).map(move |x| {
                log::info!("Transfer end: '{}'", path);
                x
            }))
            .boxed(),
        )
        .unwrap();
    return Ok(());
}
