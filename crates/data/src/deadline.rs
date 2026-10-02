//! The interactive client's bound: one deadline over a whole exchange.
//!
//! A person who confirmed a write waits for connecting, the answer's headers
//! and every byte of its body. tower's `Timeout` stops at the headers and
//! tower-http's body timeout bounds only the gap between frames, so neither
//! bounds that wait. [`Deadline`] runs the call and collects the body under
//! one `tokio::time::timeout_at`, which is only right because the interactive
//! client never streams: exec sessions, forwards, log follows and watches stay
//! on the sync client, where a total deadline would end them. Connecting is
//! inside that one future because hyper-util's client connects inside the
//! call it returns; a TLS handshake a listener accepts and never answers
//! proves it (connect_test).

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use http_body_util::{BodyExt as _, Full};
use kube::client::{Body, DynBody};
use tower::{BoxError, Layer, Service};

/// No complete answer arrived within the deadline. It travels inside kube's
/// `Error::Service`, whose own Display would read "ServiceError: ...", so
/// [`crate::connect::describe`] finds it in the chain and prints it as is.
#[derive(Debug)]
pub struct NoAnswer(pub Duration);

impl std::fmt::Display for NoAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let NoAnswer(deadline) = self;
        if deadline.subsec_millis() == 0 {
            write!(f, "no answer within {} s", deadline.as_secs())
        } else {
            write!(f, "no answer within {} ms", deadline.as_millis())
        }
    }
}

impl std::error::Error for NoAnswer {}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Deadline(pub(crate) Duration);

impl<S> Layer<S> for Deadline {
    type Service = Bounded<S>;

    fn layer(&self, inner: S) -> Bounded<S> {
        Bounded {
            inner,
            deadline: self.0,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Bounded<S> {
    inner: S,
    deadline: Duration,
}

impl<S> Service<http::Request<Body>> for Bounded<S>
where
    S: Service<http::Request<Body>, Response = http::Response<Box<DynBody>>, Error = BoxError>,
    S::Future: Send + 'static,
{
    type Response = http::Response<Box<DynBody>>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), BoxError>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: http::Request<Body>) -> Self::Future {
        let deadline = self.deadline;
        let until = tokio::time::Instant::now() + deadline;
        let answer = self.inner.call(request);
        Box::pin(async move {
            let exchange = async {
                let (parts, body) = answer.await?.into_parts();
                let bytes = body.collect().await?.to_bytes();
                let body: Box<DynBody> = Box::new(Full::new(bytes).map_err(|never| match never {}));
                Ok::<_, BoxError>(http::Response::from_parts(parts, body))
            };
            tokio::time::timeout_at(until, exchange)
                .await
                .map_err(|_| Box::new(NoAnswer(deadline)) as BoxError)?
        })
    }
}
