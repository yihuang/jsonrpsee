use crate::JsonValue;
use crate::raw_server::http::response;
use crate::raw_server::{RawServerRef, RawServerRefRq};
use futures::{channel::mpsc, channel::oneshot, lock::Mutex, prelude::*};
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Error, Response};
use std::{io, net::ToSocketAddrs, pin::Pin, thread};

// Implementation note: hyper's API is not adapted to async/await at all, and there's
// unfortunately a lot of boilerplate here that could be removed once/if it gets reworked.

pub struct HttpServer {
    rx: Mutex<mpsc::Receiver<HttpServerRefRq>>,
}

impl HttpServer {
    // TODO: `ToSocketAddrs` can be blocking
    pub fn bind(addr: impl ToSocketAddrs) -> HttpServer {
        let (mut tx, rx) = mpsc::channel(4);

        let addr = addr.to_socket_addrs().unwrap().next().unwrap(); // TODO: no

        let make_service = make_service_fn(move |_| {
            let mut tx = tx.clone();
            async move {
                Ok::<_, Error>(service_fn(move |req| {
                    let mut tx = tx.clone();
                    async move { Ok::<_, Error>(process_request(req, &mut tx).await) }
                }))
            }
        });

        let server = hyper::Server::bind(&addr).serve(make_service);

        // Because hyper can only be polled through tokio, we spawn it in a background thread.
        thread::spawn(move || {
            let mut runtime = tokio::runtime::current_thread::Runtime::new().unwrap();
            runtime.block_on(async move {
                //future::select(shutdown_rx, server);
                if let Err(err) = server.await {
                    panic!("{:?}", err);
                    // TODO: log
                }
                panic!("HTTP server closed");
            });
        });

        HttpServer { rx: Mutex::new(rx) }
    }
}

impl<'a> RawServerRef<'a> for &'a HttpServer {
    type RawServerRefRq = HttpServerRefRq;
    type Future = Pin<Box<dyn Future<Output = Result<Self::RawServerRefRq, ()>> + Send + 'a>>;

    fn next_payload(self) -> Self::Future {
        Box::pin(async move {
            let mut rx = self.rx.lock().await;
            rx.next().await.ok_or_else(|| ())
        })
    }
}

/// HTTP request that must be answered.
pub struct HttpServerRefRq {
    send_back: oneshot::Sender<hyper::Response<hyper::Body>>,
    /// The JSON body that was sent by the user.
    json: JsonValue,
}

impl RawServerRefRq for HttpServerRefRq {
    type SendBackFut = Pin<Box<dyn Future<Output = Result<(), io::Error>> + Send>>;

    fn json(&self) -> &JsonValue {
        &self.json
    }

    fn respond(self, response: &JsonValue) -> Self::SendBackFut {
        let serialization_result = serde_json::to_vec(response);

        Box::pin(async move {
            let bytes = match serialization_result {
                Ok(b) => b,
                Err(_) => panic!()      // TODO: no
            };

            let response = hyper::Response::builder()
                .status(hyper::StatusCode::OK)
                .header(
                    "Content-Type",
                    hyper::header::HeaderValue::from_static("application/json; charset=utf-8"),
                )
                .body(hyper::Body::from(bytes))
                .expect("Unable to parse response body for type conversion");

            self.send_back.send(response).map_err(|_| io::Error::from(io::ErrorKind::Other));      // TODO:
            Ok(())
        })
    }
}

/// Process an HTTP request and sends back a response.
///
/// This function is the main method invoked whenever we receive an HTTP request.
///
/// In order to process JSON-RPC requests, it has access to `fg_process_tx`. Objects sent on this
/// channel will be dispatched to the user.
async fn process_request(
    request: hyper::Request<hyper::Body>,
    fg_process_tx: &mut mpsc::Sender<HttpServerRefRq>,
) -> hyper::Response<hyper::Body> {
    /*if self.cors_allow_origin == cors::AllowCors::Invalid && !continue_on_invalid_cors {
        return response::invalid_allow_origin();
    }

    if self.cors_allow_headers == cors::AllowCors::Invalid && !continue_on_invalid_cors {
        return response::invalid_allow_headers();
    }

    // Read metadata
    let metadata = self.jsonrpc_handler.extractor.read_metadata(&request);*/

    // Proceed
    match *request.method() {
        // Validate the ContentType header
        // to prevent Cross-Origin XHRs with text/plain
        hyper::Method::POST if is_json(request.headers().get("content-type")) => {
            let uri = //if self.rest_api != RestApi::Disabled {
                Some(request.uri().clone())
            /*} else {
                None
            }*/;

            let json_body = body_to_json(request.into_body()).await;

            let (tx, rx) = oneshot::channel();
            let user_facing_rq = HttpServerRefRq {
                send_back: tx,
                json: JsonValue::Null,      // FIXME:
            };
            if let Err(_) = fg_process_tx.send(user_facing_rq).await {
                return response::internal_error("JSON requests processing channel has shut down");
            }
            match rx.await {
                Ok(response) => response,
                Err(_) => return response::internal_error("JSON request send back channel has shut down"),
            }
        }
        /*Method::POST if /*self.rest_api == RestApi::Unsecure &&*/ request.uri().path().split('/').count() > 2 => {
            RpcHandlerState::ProcessRest {
                metadata,
                uri: request.uri().clone(),
            }
        }
        // Just return error for unsupported content type
        Method::POST => response::unsupported_content_type(),
        // Don't validate content type on options
        Method::OPTIONS => response::empty(),
        // Respond to health API request if there is one configured.
        Method::GET if self.health_api.as_ref().map(|x| &*x.0) == Some(request.uri().path()) => {
            RpcHandlerState::ProcessHealth {
                metadata,
                method: self
                    .health_api
                    .as_ref()
                    .map(|x| x.1.clone())
                    .expect("Health api is defined since the URI matched."),
            }
        }*/
        // Disallow other methods.
        _ => response::method_not_allowed(),
    }
}

/// Returns true if the `content_type` header indicates a valid JSON message.
fn is_json(content_type: Option<&hyper::header::HeaderValue>) -> bool {
    match content_type.and_then(|val| val.to_str().ok()) {
        Some(ref content)
            if content.eq_ignore_ascii_case("application/json")
                || content.eq_ignore_ascii_case("application/json; charset=utf-8")
                || content.eq_ignore_ascii_case("application/json;charset=utf-8") =>
        {
            true
        }
        _ => false,
    }
}

/// Converts a `hyper` body into a structured JSON object.
///
/// Enforces a size limit on the body.
async fn body_to_json(mut body: hyper::Body) -> Result<JsonValue, io::Error> {
    let mut json_body = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(err) => return Err(io::Error::new(io::ErrorKind::Other, err.to_string())),      // TODO:
        };
        json_body.extend_from_slice(&chunk.into_bytes());
        if json_body.len() >= 16384 {       // TODO: some limit
            return Err(io::Error::new(io::ErrorKind::Other, "request too large"));
        }
    }

    Ok(serde_json::from_slice(&json_body)?)
}

#[cfg(test)]
mod tests {
    use super::body_to_json;

    #[test]
    fn body_to_json_works() {
        futures::executor::block_on(async move {
            let mut body = hyper::Body::from("[{\"a\":\"hello\"}]");
            let json = body_to_json(body).await.unwrap();
            assert_eq!(json, serde_json::Value::from(vec![
                std::iter::once((
                    "a".to_string(),
                    serde_json::Value::from("hello")
                )).collect::<serde_json::Map<_, _>>()
            ]));
        });
    }

    #[test]
    fn body_to_json_size_limit_json() {
        let huge_body = serde_json::to_vec(
            &(0..32768).map(|_| serde_json::Value::from("test")).collect::<Vec<_>>()
        ).unwrap();

        futures::executor::block_on(async move {
            let mut body = hyper::Body::from(huge_body);
            assert!(body_to_json(body).await.is_err());
        });
    }

    #[test]
    fn body_to_json_size_limit_garbage() {
        let huge_body = (0..100_000).map(|_| rand::random::<u8>()).collect::<Vec<_>>();
        futures::executor::block_on(async move {
            let mut body = hyper::Body::from(huge_body);
            assert!(body_to_json(body).await.is_err());
        });
    }
}