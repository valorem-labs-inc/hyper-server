//! Future types.
use crate::accept::Accept;
use crate::proxy_protocol::ForwardClientIp;
use std::{
    fmt,
    future::Future,
    io,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};

use pin_project_lite::pin_project;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::Timeout;

pin_project! {
    /// Future type for [`ProxyProtocolAcceptor`](crate::proxy_protocol::ProxyProtocolAcceptor).
    pub struct ProxyProtocolAcceptorFuture<F, A, I, S>
    where
        A: Accept<I, S>,
    {
        #[pin]
        inner: AcceptFuture<F, A, I, S>,
    }
}

impl<F, A, I, S> ProxyProtocolAcceptorFuture<F, A, I, S>
where
    A: Accept<I, S>,
    I: AsyncRead + AsyncWrite + Unpin,
{
    pub(crate) fn new(future: Timeout<F>, acceptor: A, service: S) -> Self {
        let inner = AcceptFuture::ReadHeader {
            future,
            acceptor,
            service: Some(service),
        };
        Self { inner }
    }
}

impl<F, A, I, S> fmt::Debug for ProxyProtocolAcceptorFuture<F, A, I, S>
where
    A: Accept<I, S>,
    I: AsyncRead + AsyncWrite + Unpin,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyProtocolAcceptorFuture").finish()
    }
}

pin_project! {
    #[project = AcceptFutureProj]
    enum AcceptFuture<F, A, I, S>
    where
        A: Accept<I, S>,
    {
        ReadHeader {
            #[pin]
            future: Timeout<F>,
            acceptor: A,
            service: Option<S>,
        },
        ForwardIp {
            #[pin]
            future: A::Future,
            client_address_opt: Option<SocketAddr>,
        },
    }
}

impl<F, A, I, S> Future for ProxyProtocolAcceptorFuture<F, A, I, S>
where
    A: Accept<I, S>,
    I: AsyncRead + AsyncWrite + Unpin,
    F: Future<Output = Result<(I, Option<SocketAddr>), io::Error>>,
{
    type Output = io::Result<(A::Stream, ForwardClientIp<A::Service>)>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();

        loop {
            match this.inner.as_mut().project() {
                AcceptFutureProj::ReadHeader {
                    future,
                    acceptor,
                    service,
                } => match future.poll(cx) {
                    Poll::Ready(Ok(Ok((stream, client_address_opt)))) => {
                        let service = service.take().expect("future polled after ready");
                        let future = acceptor.accept(stream, service);

                        this.inner.set(AcceptFuture::ForwardIp {
                            future,
                            client_address_opt,
                        });
                    }
                    Poll::Ready(Ok(Err(e))) => return Poll::Ready(Err(e)),
                    Poll::Ready(Err(timeout)) => {
                        return Poll::Ready(Err(io::Error::new(io::ErrorKind::TimedOut, timeout)))
                    }
                    Poll::Pending => return Poll::Pending,
                },
                AcceptFutureProj::ForwardIp {
                    future,
                    client_address_opt,
                } => match future.poll(cx) {
                    Poll::Ready(Ok((stream, service))) => {
                        let service = ForwardClientIp {
                            inner: service,
                            client_address_opt: *client_address_opt,
                        };

                        return Poll::Ready(Ok((stream, service)));
                    }
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => return Poll::Pending,
                },
            }
        }
    }
}
