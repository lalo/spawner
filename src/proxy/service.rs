use crate::database::DroneDatabase;
use anyhow::Result;
use http::uri::{Authority, Scheme};
use http::Uri;
use hyper::client::HttpConnector;
use hyper::Client;
use hyper::{service::Service, Body, Request, Response, StatusCode};
use std::str::FromStr;
use std::{
    convert::Infallible,
    future::{ready, Future, Ready},
    pin::Pin,
    task::Poll,
};

pub struct MakeProxyService {
    db: DroneDatabase,
    client: Client<HttpConnector, Body>,
}

impl MakeProxyService {
    pub fn new(db: DroneDatabase) -> Self {
        MakeProxyService { db, client: Client::new() }
    }
}

impl<T> Service<T> for MakeProxyService {
    type Response = ProxyService;
    type Error = Infallible;
    type Future = Ready<Result<ProxyService, Infallible>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _req: T) -> Self::Future {
        ready(Ok(ProxyService {
            db: self.db.clone(),
            client: self.client.clone(),
        }))
    }
}

#[derive(Clone)]
pub struct ProxyService {
    db: DroneDatabase,
    client: Client<HttpConnector, Body>,
}

#[allow(unused)]
impl ProxyService {
    fn rewrite_uri(authority: &str, uri: &Uri) -> anyhow::Result<Uri> {
        let mut parts = uri.clone().into_parts();
        parts.authority = Some(Authority::from_str(authority)?);
        parts.scheme = Some(Scheme::HTTP);
        let uri = Uri::from_parts(parts)?;

        Ok(uri)
    }

    async fn handle(self, mut req: Request<Body>) -> anyhow::Result<Response<Body>> {
        if let Some(host) = req.headers().get(http::header::HOST) {
            let host = std::str::from_utf8(&host.as_bytes())?;

            if let Some(addr) = self.db.get_proxy_route(host).await? {
                *req.uri_mut() = Self::rewrite_uri(&addr, req.uri())?;

                let result = self.client.request(req).await;
                return Ok(result?);
            }

            tracing::warn!(?host, "Unrecognized host.");
        }

        Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::empty())?)
    }

    async fn warn_handle(self, mut req: Request<Body>) -> anyhow::Result<Response<Body>> {
        let result = self.handle(req).await;

        match &result {
            Err(error) => tracing::warn!(?error, "Error handling request."),
            _ => (),
        }

        result
    }
}

type ProxyServiceFuture =
    Pin<Box<dyn Future<Output = Result<Response<Body>, anyhow::Error>> + Send + 'static>>;

impl Service<Request<Body>> for ProxyService {
    type Response = Response<Body>;
    type Error = anyhow::Error;
    type Future = ProxyServiceFuture;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        Box::pin(self.clone().warn_handle(req))
    }
}
