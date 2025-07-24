use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use actix_web::body::{BodySize, BoxBody, MessageBody};
use actix_web::http::header::{CacheControl, CacheDirective, ContentEncoding};
use actix_web::{HttpRequest, HttpResponse, Responder};
use bytes::Bytes;
use futures_core::Stream;
use pin_project_lite::pin_project;
use tokio::time::{Interval, interval};

use crate::Event;

type BoxError = Box<dyn std::error::Error>;

pin_project! {
    /// Server-sent events (`text/event-stream`) responder.
    ///
    /// Constructed using a [Tokio channel](Self::from_receiver) or using your [own
    /// stream](Self::from_stream).
    #[must_use]
    #[derive(Debug)]
    pub struct Sse<S> {
        #[pin]
        stream: S,
        keep_alive: Option<Interval>,
        retry_interval: Option<Duration>,
    }
}

impl<S, E> Sse<S>
where
    S: Stream<Item = Result<Event, E>> + 'static,
    E: Into<BoxError>,
{
    /// Create an SSE response from a stream that yields SSE [Event]s.
    pub fn from_stream(stream: S) -> Self {
        Self {
            stream,
            keep_alive: None,
            retry_interval: None,
        }
    }
}

impl<S> Sse<S> {
    /// Enables "keep-alive" messages to be send in the event stream after a period of inactivity.
    ///
    /// By default, no keep-alive is set up.
    pub fn with_keep_alive(mut self, keep_alive_period: Duration) -> Self {
        let mut int = interval(keep_alive_period);
        int.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        self.keep_alive = Some(int);
        self
    }

    /// Queues first event message to inform client of custom retry period.
    ///
    /// Browsers default to retry every 3 seconds or so.
    pub fn with_retry_duration(mut self, retry: Duration) -> Self {
        self.retry_interval = Some(retry);
        self
    }
}

impl<S, E> Responder for Sse<S>
where
    S: Stream<Item = Result<Event, E>> + 'static,
    E: Into<BoxError>,
{
    type Body = BoxBody;

    fn respond_to(self, _req: &HttpRequest) -> HttpResponse<Self::Body> {
        HttpResponse::Ok()
            .content_type(mime::TEXT_EVENT_STREAM)
            .insert_header(ContentEncoding::Identity)
            .insert_header(CacheControl(vec![CacheDirective::NoCache]))
            .body(self)
    }
}

impl<S, E> MessageBody for Sse<S>
where
    S: Stream<Item = Result<Event, E>>,
    E: Into<BoxError>,
{
    type Error = BoxError;

    fn size(&self) -> BodySize {
        BodySize::Stream
    }

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, Self::Error>>> {
        let this = self.project();

        if let Some(retry) = this.retry_interval.take() {
            cx.waker().wake_by_ref();
            return Poll::Ready(Some(Ok(Event::retry_to_bytes(retry))));
        }

        if let Poll::Ready(msg) = this.stream.poll_next(cx) {
            return match msg {
                Some(Ok(msg)) => Poll::Ready(Some(Ok(msg.into_bytes()))),
                Some(Err(err)) => Poll::Ready(Some(Err(err.into()))),
                None => Poll::Ready(None),
            };
        }

        if let Some(keep_alive) = this.keep_alive {
            if keep_alive.poll_tick(cx).is_ready() {
                return Poll::Ready(Some(Ok(Event::keep_alive_bytes())));
            }
        }

        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::task::ready;

    use actix_web::body;
    use actix_web::http::StatusCode;
    use actix_web::test::TestRequest;
    use futures_util::future::poll_fn;
    use futures_util::task::noop_waker;
    use futures_util::{FutureExt as _, StreamExt as _, stream};
    use tokio::time::sleep;
    use tokio_stream::wrappers::ReceiverStream;

    use super::*;
    use crate::Data;

    pin_project_lite::pin_project! {
        /// Converts stream with item `T` into `Result<T, Infallible>`.
        pub struct InfallibleStream<S> {
            #[pin]
            stream: S,
        }
    }

    impl<S> InfallibleStream<S> {
        pub fn new(stream: S) -> Self {
            Self { stream }
        }
    }

    impl<S: Stream> Stream for InfallibleStream<S> {
        type Item = Result<S::Item, Infallible>;

        fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(ready!(self.project().stream.poll_next(cx)).map(Ok))
        }

        fn size_hint(&self) -> (usize, Option<usize>) {
            self.stream.size_hint()
        }
    }

    #[test]
    fn retry_is_first_msg() {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let mut sse = Sse::from_stream(stream::empty::<Result<_, Infallible>>())
            .with_retry_duration(Duration::from_millis(42));
        match Pin::new(&mut sse).poll_next(&mut cx) {
            Poll::Ready(Some(Ok(bytes))) => assert_eq!(bytes, "retry: 42\n\n"),
            res => panic!("poll should return retry message, got {res:?}"),
        }
    }

    #[actix_web::test]
    async fn sse_from_external_streams() {
        let st = stream::empty::<Result<_, Infallible>>();
        let sse = Sse::from_stream(st);
        assert_eq!(body::to_bytes(sse).await.unwrap(), "");

        let st = stream::once(async { Ok::<_, Infallible>(Event::Data(Data::new("foo"))) });
        let sse = Sse::from_stream(st);
        assert_eq!(body::to_bytes(sse).await.unwrap(), "data: foo\n\n");

        let st = stream::repeat(Ok::<_, Infallible>(Event::Data(Data::new("foo")))).take(2);
        let sse = Sse::from_stream(st);
        assert_eq!(
            body::to_bytes(sse).await.unwrap(),
            "data: foo\n\ndata: foo\n\n",
        );
    }

    #[actix_web::test]
    async fn appropriate_headers_are_set_on_responder() {
        let st = stream::empty::<Result<_, Infallible>>();
        let sse = Sse::from_stream(st);

        let res = sse.respond_to(&TestRequest::default().to_http_request());

        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers().get("content-type").unwrap(),
            "text/event-stream"
        );
        assert_eq!(res.headers().get("content-encoding").unwrap(), "identity");
        assert_eq!(res.headers().get("cache-control").unwrap(), "no-cache");
    }

    #[actix_web::test]
    async fn messages_are_received_from_sender() {
        let (sender, receiver) = tokio::sync::mpsc::channel(2);
        let stream = InfallibleStream::new(ReceiverStream::new(receiver));
        let mut sse = Sse::from_stream(stream);

        assert!(
            poll_fn(|cx| Pin::new(&mut sse).poll_next(cx))
                .now_or_never()
                .is_none()
        );

        sender
            .send(Data::new("bar").event("foo").into())
            .await
            .unwrap();

        match poll_fn(|cx| Pin::new(&mut sse).poll_next(cx)).now_or_never() {
            Some(Some(Ok(bytes))) => assert_eq!(bytes, "event: foo\ndata: bar\n\n"),
            res => panic!("poll should return data message, got {res:?}"),
        }
    }

    #[actix_web::test]
    async fn keep_alive_is_sent() {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let (sender, receiver) = tokio::sync::mpsc::channel(2);
        let stream = InfallibleStream::new(ReceiverStream::new(receiver));
        let mut sse = Sse::from_stream(stream).with_keep_alive(Duration::from_millis(4));

        assert!(Pin::new(&mut sse).poll_next(&mut cx).is_pending());

        sleep(Duration::from_millis(20)).await;

        match Pin::new(&mut sse).poll_next(&mut cx) {
            Poll::Ready(Some(Ok(bytes))) => assert_eq!(bytes, ": keep-alive\n\n"),
            res => panic!("poll should return data message, got {res:?}"),
        }

        assert!(Pin::new(&mut sse).poll_next(&mut cx).is_pending());

        sender.send(Data::new("foo").into()).await.unwrap();

        match Pin::new(&mut sse).poll_next(&mut cx) {
            Poll::Ready(Some(Ok(bytes))) => assert_eq!(bytes, "data: foo\n\n"),
            res => panic!("poll should return data message, got {res:?}"),
        }
    }
}
