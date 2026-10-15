//! The interactive client's bound: one deadline over a whole exchange.
//!
//! A person who confirmed a write waits for connecting, the answer's headers
//! and every byte of its body. tower's `Timeout` stops at the headers and
//! tower-http's body timeout bounds only the gap between frames, so neither
//! bounds that wait. [`Deadline`] bounds the call and every body frame by the
//! same absolute instant. Returning the headers without collecting the body
//! lets bounded readers stop at their own cap and refusals drop an unread
//! body. Exec sessions, forwards, log follows and watches stay on the sync
//! client, where a total deadline would end them. Connecting is inside the
//! call because hyper-util's client connects inside the future it returns; a
//! TLS handshake a listener accepts and never answers proves it (connect_test).

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use http_body_util::StreamBody;
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
            let (parts, body) = tokio::time::timeout_at(until, answer)
                .await
                .map_err(|_| Box::new(NoAnswer(deadline)) as BoxError)??
                .into_parts();
            if body.is_end_stream() {
                return Ok(http::Response::from_parts(parts, body));
            }
            let mut state = Some((body, Box::pin(tokio::time::sleep_until(until))));
            let frames = futures::stream::poll_fn(move |cx| {
                let Some((body, timer)) = state.as_mut() else {
                    return Poll::Ready(None);
                };
                if tokio::time::Instant::now() >= until || timer.as_mut().poll(cx).is_ready() {
                    state.take();
                    return Poll::Ready(Some(Err(Box::new(NoAnswer(deadline)) as BoxError)));
                }
                let frame = Pin::new(body.as_mut()).poll_frame(cx);
                if matches!(&frame, Poll::Ready(None | Some(Err(_)))) || body.is_end_stream() {
                    state.take();
                }
                frame
            });
            let body: Box<DynBody> = Box::new(StreamBody::new(frames));
            Ok(http::Response::from_parts(parts, body))
        })
    }
}

#[cfg(test)]
#[path = "deadline_test.rs"]
mod tests;
