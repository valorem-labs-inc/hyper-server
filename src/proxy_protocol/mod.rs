//! This feature allows the `hyper_server` to be used behind a layer 4 load balancer whilst the proxy
//! protocol is enabled to preserve the client IP address and port.
//! See The PROXY protocol spec for more details: <https://www.haproxy.org/download/1.8/doc/proxy-protocol.txt>.
//!
//! Any client address found in the proxy protocol header is forwarded on in the HTTP `forwarded`
//! header to be accessible by the rest server.
//!
//! Note: if you are setting a custom acceptor, `enable_proxy_protocol` must be called after this is set.
//! It is best to use directly before calling `serve` when the inner acceptor is already configured.
//! `ProxyProtocolAcceptor` wraps the initial acceptor, so the proxy header is removed from the
//! beginning of the stream before the messages are forwarded on.
//!
//! # Example
//!
//! ```rust,no_run
//! use axum::{routing::get, Router};
//! use std::net::SocketAddr;
//!
//! #[tokio::main]
//! async fn main() {
//!    let app = Router::new().route("/", get(|| async { "Hello, world!" }));
//!
//!    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
//!    println!("listening on {}", addr);
//!    hyper_server::bind(addr)
//!        .enable_proxy_protocol()
//!        .serve(app.into_make_service())
//!        .await
//!        .unwrap();
//! }
//! ```
use crate::accept::Accept;
use std::{
    fmt,
    future::Future,
    io,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use http::HeaderValue;
use http::Request;
use ppp::{v1, v2, HeaderResult};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite},
    time::timeout,
};
use tower_service::Service;

pub mod future;
use self::future::ProxyProtocolAcceptorFuture;

const V1_PREFIX_LEN: usize = 5;
const V2_PREFIX_LEN: usize = 12;
/// The index of the version-command byte.
const V2_MINIMUM_LEN: usize = 16;
/// The index of the start of the big-endian u16 length.
const LENGTH: usize = 14;
const DEFAULT_BUFFER_LEN: usize = 512;

/// Note: Currently only supports the V2 PROXY header format.
/// See proxy header spec: <https://www.haproxy.org/download/1.8/doc/proxy-protocol.txt>
pub(crate) async fn read_proxy_header<I>(
    mut stream: I,
) -> Result<(I, Option<SocketAddr>), io::Error>
where
    I: AsyncRead + Unpin,
{
    // mutable buffer for storing stream data
    let mut buffer = [0; DEFAULT_BUFFER_LEN];

    // 1. read minimum length
    if let Err(e) = stream.read_exact(&mut buffer[..V2_MINIMUM_LEN]).await {
        let msg = format!("Stream `read_exact` error: {}. Not able to read enough bytes to find a V2 Proxy Protocol header.", e);
        return Err(io::Error::new(e.kind(), msg));
    }

    // 2. check for v2 prefix
    if &buffer[..V2_PREFIX_LEN] != v2::PROTOCOL_PREFIX {
        if &buffer[..V1_PREFIX_LEN] == v1::PROTOCOL_PREFIX.as_bytes() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "V1 Proxy Protocol header detected, which is not supported",
            ));
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "No valid Proxy Protocol header detected",
            ));
        }
    }

    // 3. decode the v2 header length
    let length = u16::from_be_bytes([buffer[LENGTH], buffer[LENGTH + 1]]) as usize;
    let full_length = V2_MINIMUM_LEN + length;

    if full_length > DEFAULT_BUFFER_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("V2 Proxy Protocol header length is too long. Consider increasing default buffer size. Header length: {}", full_length),
        ));
    }

    // 4. read the remaining header length
    if let Err(e) = stream
        .read_exact(&mut buffer[V2_MINIMUM_LEN..full_length])
        .await
    {
        let msg = format!("Stream `read_exact` error: {}. Not able to read enough bytes to read the full V2 Proxy Protocol header.", e);
        return Err(io::Error::new(e.kind(), msg));
    }

    // 5. parse the header
    let header = HeaderResult::parse(&buffer[..full_length]);

    match header {
        HeaderResult::V1(Ok(_header)) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "V1 Proxy Protocol header detected when parsing data",
        )),
        HeaderResult::V2(Ok(header)) => {
            let client_address = match header.addresses {
                v2::Addresses::IPv4(ip) => {
                    SocketAddr::new(IpAddr::V4(ip.source_address), ip.source_port)
                }
                v2::Addresses::IPv6(ip) => {
                    SocketAddr::new(IpAddr::V6(ip.source_address), ip.source_port)
                }
                v2::Addresses::Unix(unix) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "Unix socket addresses are not supported. Addresses: {:?}",
                            unix
                        ),
                    ));
                }
                v2::Addresses::Unspecified => {
                    // V2 PROXY header addresses "Unspecified"
                    // Return client address as `None` so that "unknown" is used in the http header
                    return Ok((stream, None));
                }
            };

            Ok((stream, Some(client_address)))
        }
        HeaderResult::V1(Err(_error)) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "No valid V1 Proxy Protocol header received",
        )),
        HeaderResult::V2(Err(_error)) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "No valid V2 Proxy Protocol header received",
        )),
    }
}

/// Middleware for adding client IP address to the request `forwarded` header.
/// see spec: <https://www.rfc-editor.org/rfc/rfc7239#section-5.2>
#[derive(Debug, Clone)]
pub struct ForwardClientIp<S> {
    inner: S,
    client_address: Option<SocketAddr>,
}

impl<B, S> Service<Request<B>> for ForwardClientIp<S>
where
    S: Service<Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<B>) -> Self::Future {
        // The full socket address is available in the proxy header, hence why we include port
        let mut forwarded_string = match self.client_address {
            Some(socket_addr) => match socket_addr {
                SocketAddr::V4(addr) => {
                    format!("for={}:{}", addr.ip(), addr.port())
                }
                SocketAddr::V6(addr) => {
                    format!("for=\"[{}]:{}\"", addr.ip(), addr.port())
                }
            },
            None => "for=unknown".to_string(),
        };

        if let Some(existing_value) = req.headers_mut().get("Forwarded") {
            forwarded_string = format!(
                "{}, {}",
                existing_value.to_str().unwrap_or(""),
                &forwarded_string
            );
        }

        if let Ok(header_value) = HeaderValue::from_str(&forwarded_string) {
            req.headers_mut().insert("Forwarded", header_value);
        }

        self.inner.call(req)
    }
}

/// Acceptor wrapper for receiving Proxy Protocol headers.
#[derive(Clone)]
pub struct ProxyProtocolAcceptor<A> {
    inner: A,
    parsing_timeout: Duration,
}

impl<A> ProxyProtocolAcceptor<A> {
    /// Create a new proxy protocol acceptor from an initial acceptor.
    /// This is compatible with tls acceptors.
    pub fn new(inner: A) -> Self {
        #[cfg(not(test))]
        let parsing_timeout = Duration::from_secs(10);

        // Don't force tests to wait too long.
        #[cfg(test)]
        let parsing_timeout = Duration::from_secs(1);

        Self {
            inner,
            parsing_timeout,
        }
    }

    /// Override the default Proxy Header parsing timeout of 10 seconds, except during testing.
    pub fn parsing_timeout(mut self, val: Duration) -> Self {
        self.parsing_timeout = val;
        self
    }
}

impl<A> ProxyProtocolAcceptor<A> {
    /// Overwrite inner acceptor.
    pub fn acceptor<Acceptor>(self, acceptor: Acceptor) -> ProxyProtocolAcceptor<Acceptor> {
        ProxyProtocolAcceptor {
            inner: acceptor,
            parsing_timeout: self.parsing_timeout,
        }
    }
}

impl<A, I, S> Accept<I, S> for ProxyProtocolAcceptor<A>
where
    A: Accept<I, S> + Clone,
    A::Stream: AsyncRead + AsyncWrite + Unpin,
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = A::Stream;
    type Service = ForwardClientIp<A::Service>;
    type Future = ProxyProtocolAcceptorFuture<
        Pin<Box<dyn Future<Output = Result<(I, Option<SocketAddr>), io::Error>> + Send>>,
        A,
        I,
        S,
    >;

    fn accept(&self, stream: I, service: S) -> Self::Future {
        let future = Box::pin(read_proxy_header(stream));

        ProxyProtocolAcceptorFuture::new(
            timeout(self.parsing_timeout, future),
            self.inner.clone(),
            service,
        )
    }
}

impl<A> fmt::Debug for ProxyProtocolAcceptor<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyProtocolAcceptor").finish()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    #[cfg(feature = "tls-openssl")]
    use crate::tls_openssl::{
        self,
        tests::{dns_name as openssl_dns_name, tls_connector as openssl_connector},
        OpenSSLConfig,
    };
    #[cfg(feature = "tls-rustls")]
    use crate::tls_rustls::{
        self,
        tests::{dns_name as rustls_dns_name, tls_connector as rustls_connector},
        RustlsConfig,
    };
    use crate::{handle::Handle, server::Server};
    use axum::http::Response;
    use axum::{routing::get, Router};
    use bytes::Bytes;
    use http::{response, Request};
    use hyper::{
        client::conn::{handshake, SendRequest},
        Body,
    };
    use ppp::v2::{Builder, Command, Protocol, Type, Version};
    use std::{io, net::SocketAddr, time::Duration};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::{
        net::{TcpListener, TcpStream},
        task::JoinHandle,
        time::timeout,
    };
    use tower::{Service, ServiceExt};

    #[tokio::test]
    async fn start_and_request() {
        let (_handle, _server_task, server_addr) = start_server(true).await;

        let addr = start_proxy(server_addr, true)
            .await
            .expect("Failed to start proxy");

        let (mut client, _conn, _client_addr) = connect(addr).await;

        let (_parts, body) = send_empty_request(&mut client).await;

        assert_eq!(body.as_ref(), b"Hello, world!");
    }

    #[tokio::test]
    async fn server_receives_client_address() {
        let (_handle, _server_task, server_addr) = start_server(true).await;

        let addr = start_proxy(server_addr, true)
            .await
            .expect("Failed to start proxy");

        let (mut client, _conn, client_addr) = connect(addr).await;

        let (parts, body) = send_empty_request(&mut client).await;

        // Check for the Forwarded header
        let forwarded_header = parts
            .headers
            .get("Forwarded")
            .expect("No Forwarded header present")
            .to_str()
            .expect("Failed to convert Forwarded header to str");

        assert!(forwarded_header.contains(&format!("for={}", client_addr)));
        assert_eq!(body.as_ref(), b"Hello, world!");
    }

    #[cfg(feature = "tls-rustls")]
    #[tokio::test]
    async fn rustls_server_receives_client_address() {
        let (_handle, _server_task, server_addr) = start_rustls_server().await;

        let addr = start_proxy(server_addr, true)
            .await
            .expect("Failed to start proxy");

        let (mut client, _conn, client_addr) = rustls_connect(addr).await;

        let (parts, body) = send_empty_request(&mut client).await;

        // Check for the Forwarded header
        let forwarded_header = parts
            .headers
            .get("Forwarded")
            .expect("No Forwarded header present")
            .to_str()
            .expect("Failed to convert Forwarded header to str");

        assert!(forwarded_header.contains(&format!("for={}", client_addr)));
        assert_eq!(body.as_ref(), b"Hello, world!");
    }

    #[cfg(feature = "tls-openssl")]
    #[tokio::test]
    async fn openssl_server_receives_client_address() {
        let (_handle, _server_task, server_addr) = start_openssl_server().await;

        let addr = start_proxy(server_addr, true)
            .await
            .expect("Failed to start proxy");

        let (mut client, _conn, client_addr) = openssl_connect(addr).await;

        let (parts, body) = send_empty_request(&mut client).await;

        // Check for the Forwarded header
        let forwarded_header = parts
            .headers
            .get("Forwarded")
            .expect("No Forwarded header present")
            .to_str()
            .expect("Failed to convert Forwarded header to str");

        assert!(forwarded_header.contains(&format!("for={}", client_addr)));
        assert_eq!(body.as_ref(), b"Hello, world!");
    }

    #[tokio::test]
    async fn not_parsing_when_header_present_fails() {
        let (_handle, _server_task, server_addr) = start_server(false).await;

        let addr = start_proxy(server_addr, true)
            .await
            .expect("Failed to start proxy");

        let (mut client, _conn, _client_addr) = connect(addr).await;

        let (_parts, body) = send_empty_request(&mut client).await;

        assert!(body.as_ref() != b"Hello, world!");
    }

    #[tokio::test]
    async fn parsing_when_header_not_present_fails() {
        let (_handle, _server_task, server_addr) = start_server(true).await;

        let addr = start_proxy(server_addr, false)
            .await
            .expect("Failed to start proxy");

        let (mut client, _conn, _client_addr) = connect(addr).await;

        match client
            .ready()
            .await
            .unwrap()
            .call(Request::new(Body::empty()))
            .await
        {
            Ok(_) => panic!("Should have failed"),
            Err(e) => {
                if e.is_incomplete_message() {
                } else {
                    panic!("Received unexpected error");
                }
            }
        }
    }

    pub(crate) async fn forward_ip_handler(req: Request<Body>) -> Response<Body> {
        let mut response = Response::new(Body::from("Hello, world!"));

        if let Some(header_value) = req.headers().get("Forwarded") {
            response
                .headers_mut()
                .insert("Forwarded", header_value.clone());
        }

        response
    }

    async fn start_server(
        parse_proxy_header: bool,
    ) -> (Handle, JoinHandle<io::Result<()>>, SocketAddr) {
        let handle = Handle::new();

        let server_handle = handle.clone();
        let server_task = tokio::spawn(async move {
            let app = Router::new().route("/", get(forward_ip_handler));

            let addr = SocketAddr::from(([127, 0, 0, 1], 0));

            if parse_proxy_header {
                Server::bind(addr)
                    .handle(server_handle)
                    .enable_proxy_protocol()
                    .serve(app.into_make_service())
                    .await
            } else {
                Server::bind(addr)
                    .handle(server_handle)
                    .serve(app.into_make_service())
                    .await
            }
        });

        let addr = handle.listening().await.unwrap();

        (handle, server_task, addr)
    }

    #[cfg(feature = "tls-rustls")]
    async fn start_rustls_server() -> (Handle, JoinHandle<io::Result<()>>, SocketAddr) {
        let handle = Handle::new();

        let server_handle = handle.clone();
        let server_task = tokio::spawn(async move {
            let app = Router::new().route("/", get(forward_ip_handler));

            let config = RustlsConfig::from_pem_file(
                "examples/self-signed-certs/cert.pem",
                "examples/self-signed-certs/key.pem",
            )
            .await?;

            let addr = SocketAddr::from(([127, 0, 0, 1], 0));

            tls_rustls::bind_rustls(addr, config)
                .handle(server_handle)
                .enable_proxy_protocol()
                .serve(app.into_make_service())
                .await
        });

        let addr = handle.listening().await.unwrap();

        (handle, server_task, addr)
    }

    #[cfg(feature = "tls-openssl")]
    async fn start_openssl_server() -> (Handle, JoinHandle<io::Result<()>>, SocketAddr) {
        let handle = Handle::new();

        let server_handle = handle.clone();
        let server_task = tokio::spawn(async move {
            let app = Router::new().route("/", get(forward_ip_handler));

            let config = OpenSSLConfig::from_pem_file(
                "examples/self-signed-certs/cert.pem",
                "examples/self-signed-certs/key.pem",
            )
            .unwrap();

            let addr = SocketAddr::from(([127, 0, 0, 1], 0));

            tls_openssl::bind_openssl(addr, config)
                .handle(server_handle)
                .enable_proxy_protocol()
                .serve(app.into_make_service())
                .await
        });

        let addr = handle.listening().await.unwrap();

        (handle, server_task, addr)
    }

    pub(crate) async fn start_proxy(
        server_address: SocketAddr,
        enable_proxy_header: bool,
    ) -> Result<SocketAddr, Box<dyn std::error::Error>> {
        let proxy_address = SocketAddr::from(([127, 0, 0, 1], 0));
        let listener = TcpListener::bind(proxy_address).await?;
        let proxy_address = listener.local_addr()?;

        let _proxy_task = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((client_stream, _)) => {
                        tokio::spawn(async move {
                            if let Err(e) =
                                handle_conn(client_stream, server_address, enable_proxy_header)
                                    .await
                            {
                                println!("Error handling connection: {:?}", e);
                            }
                        });
                    }
                    Err(e) => println!("Failed to accept a connection: {:?}", e),
                }
            }
        });

        Ok(proxy_address)
    }

    async fn handle_conn(
        mut client_stream: TcpStream,
        server_address: SocketAddr,
        enable_proxy_header: bool,
    ) -> io::Result<()> {
        let client_address = client_stream.peer_addr()?; // Get the address before splitting
        let mut server_stream = TcpStream::connect(server_address).await?;
        let server_address = server_stream.peer_addr()?; // Get the address before splitting

        let (mut client_read, mut client_write) = client_stream.split();
        let (mut server_read, mut server_write) = server_stream.split();

        if enable_proxy_header {
            send_proxy_header(&mut server_write, client_address, server_address).await?;
        }

        let duration = Duration::from_secs(1);
        let client_to_server = async {
            match timeout(duration, transfer(&mut client_read, &mut server_write)).await {
                Ok(result) => result,
                Err(_) => Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Client to Server transfer timed out",
                )),
            }
        };

        let server_to_client = async {
            match timeout(duration, transfer(&mut server_read, &mut client_write)).await {
                Ok(result) => result,
                Err(_) => Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Server to Client transfer timed out",
                )),
            }
        };

        let _ = tokio::try_join!(client_to_server, server_to_client);

        Ok(())
    }

    async fn transfer(
        read_stream: &mut (impl AsyncReadExt + Unpin),
        write_stream: &mut (impl AsyncWriteExt + Unpin),
    ) -> io::Result<()> {
        let mut buf = [0; 4096];
        loop {
            let n = read_stream.read(&mut buf).await?;
            if n == 0 {
                break; // EOF
            }
            write_stream.write_all(&buf[..n]).await?;
        }
        Ok(())
    }

    async fn send_proxy_header(
        write_stream: &mut (impl AsyncWriteExt + Unpin),
        client_address: SocketAddr,
        server_address: SocketAddr,
    ) -> io::Result<()> {
        let mut header = Builder::with_addresses(
            // Declare header as mutable
            Version::Two | Command::Proxy,
            Protocol::Stream,
            (client_address, server_address),
        )
        .write_tlv(Type::NoOp, b"Hello, World!")?
        .build()?;

        for byte in header.drain(..) {
            write_stream.write_all(&[byte]).await?;
        }

        Ok(())
    }

    async fn connect(addr: SocketAddr) -> (SendRequest<Body>, JoinHandle<()>, SocketAddr) {
        let stream = TcpStream::connect(addr).await.unwrap();
        let client_addr = stream.local_addr().unwrap();

        let (send_request, connection) = handshake(stream).await.unwrap();

        let task = tokio::spawn(async move {
            let _ = connection.await;
        });

        (send_request, task, client_addr)
    }

    #[cfg(feature = "tls-rustls")]
    async fn rustls_connect(addr: SocketAddr) -> (SendRequest<Body>, JoinHandle<()>, SocketAddr) {
        let stream = TcpStream::connect(addr).await.unwrap();
        let client_addr = stream.local_addr().unwrap();
        let tls_stream = rustls_connector()
            .connect(rustls_dns_name(), stream)
            .await
            .unwrap();

        let (send_request, connection) = handshake(tls_stream).await.unwrap();

        let task = tokio::spawn(async move {
            let _ = connection.await;
        });

        (send_request, task, client_addr)
    }

    #[cfg(feature = "tls-openssl")]
    async fn openssl_connect(addr: SocketAddr) -> (SendRequest<Body>, JoinHandle<()>, SocketAddr) {
        let stream = TcpStream::connect(addr).await.unwrap();
        let client_addr = stream.local_addr().unwrap();
        let tls_stream = openssl_connector(openssl_dns_name(), stream).await;

        let (send_request, connection) = handshake(tls_stream).await.unwrap();

        let task = tokio::spawn(async move {
            let _ = connection.await;
        });

        (send_request, task, client_addr)
    }

    async fn send_empty_request(client: &mut SendRequest<Body>) -> (response::Parts, Bytes) {
        let (parts, body) = client
            .ready()
            .await
            .unwrap()
            .call(Request::new(Body::empty()))
            .await
            .unwrap()
            .into_parts();
        let body = hyper::body::to_bytes(body).await.unwrap();

        (parts, body)
    }
}
