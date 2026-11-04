// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0
// Adapted from smoldot-light `src/platform/default.rs`, Copyright (C) Parity Technologies (UK) Ltd.

//! smoldot-light's `DefaultPlatform` plus secure WebSocket (`/wss`).
//!
//! `DefaultPlatform` declines `/wss` addresses (`TODO: support WebSocket
//! secure`), and every bootnode of these chain specs is `/wss`, like the ones a
//! browser host dials. This copy adds a TLS layer under the WebSocket. The rest
//! follows `smoldot-light/src/platform/default.rs`.

use std::{
    borrow::Cow,
    fmt::{self, Write as _},
    io,
    net::SocketAddr,
    panic,
    pin::Pin,
    sync::Arc,
    thread,
    time::{Duration, Instant, UNIX_EPOCH},
};

use futures_rustls::{
    TlsConnector,
    rustls::{ClientConfig, RootCertStore, pki_types::ServerName},
};
use futures_util::{AsyncRead, AsyncWrite, FutureExt as _, future};

use crate::smoldot::libp2p::websocket;
use crate::smoldot_light::platform::{
    Address, ConnectionType, LogLevel, MultiStreamAddress, MultiStreamWebRtcConnection,
    PlatformRef, SubstreamDirection, with_buffers,
};

pub trait Io: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Io for T {}

type Socket = Box<dyn Io>;

/// Cheap to clone; all clones share one executor.
#[derive(Clone)]
pub struct Platform(Arc<Inner>);

pub struct Inner {
    executor: Arc<smol::Executor<'static>>,
    shutdown: event_listener::Event,
    // `PlatformRef` must be unwind safe; rustls' trait objects are not marked so.
    tls: panic::AssertUnwindSafe<TlsConnector>,
}

impl Platform {
    pub fn new() -> Self {
        let executor = Arc::new(smol::Executor::new());
        let shutdown = event_listener::Event::new();
        for n in 0..thread::available_parallelism().map_or(4, |n| n.get()) {
            let on_shutdown = shutdown.listen();
            let executor = executor.clone();
            thread::Builder::new()
                .name(format!("smoldot-light-{n}"))
                .spawn(move || smol::block_on(executor.run(on_shutdown)))
                .unwrap();
        }
        let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        Platform(Arc::new(Inner {
            executor,
            shutdown,
            tls: panic::AssertUnwindSafe(TlsConnector::from(Arc::new(config))),
        }))
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.shutdown.notify(usize::MAX);
    }
}

#[pin_project::pin_project]
pub struct Stream(
    #[pin]
    with_buffers::WithBuffers<future::BoxFuture<'static, Result<Socket, io::Error>>, Socket, Instant>,
);

impl PlatformRef for Platform {
    type Delay = future::Map<smol::Timer, fn(Instant) -> ()>;
    type Instant = Instant;
    type MultiStream = std::convert::Infallible;
    type Stream = Stream;
    type StreamConnectFuture = future::Ready<Self::Stream>;
    type MultiStreamConnectFuture = future::Pending<MultiStreamWebRtcConnection<Self::MultiStream>>;
    type ReadWriteAccess<'a> = with_buffers::ReadWriteAccess<'a, Instant>;
    type StreamUpdateFuture<'a> = future::BoxFuture<'a, ()>;
    type StreamErrorRef<'a> = &'a io::Error;
    type NextSubstreamFuture<'a> = future::Pending<Option<(Self::Stream, SubstreamDirection)>>;

    fn now_from_unix_epoch(&self) -> Duration {
        UNIX_EPOCH.elapsed().unwrap()
    }

    fn now(&self) -> Instant {
        Instant::now()
    }

    fn fill_random_bytes(&self, buffer: &mut [u8]) {
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), buffer);
    }

    fn sleep(&self, duration: Duration) -> Self::Delay {
        smol::Timer::after(duration).map(|_| ())
    }

    fn sleep_until(&self, when: Instant) -> Self::Delay {
        smol::Timer::at(when).map(|_| ())
    }

    fn spawn_task(&self, _name: Cow<str>, task: impl Future<Output = ()> + Send + 'static) {
        let keep_alive = self.clone();
        self.0.executor
            .spawn(
                panic::AssertUnwindSafe(async move {
                    task.await;
                    drop(keep_alive);
                })
                .catch_unwind(),
            )
            .detach();
    }

    fn log<'a>(
        &self,
        level: LogLevel,
        target: &'a str,
        message: &'a str,
        key_values: impl Iterator<Item = (&'a str, &'a dyn fmt::Display)>,
    ) {
        let level = match level {
            LogLevel::Error => log::Level::Error,
            LogLevel::Warn => log::Level::Warn,
            LogLevel::Info => log::Level::Info,
            LogLevel::Debug => log::Level::Debug,
            LogLevel::Trace => log::Level::Trace,
        };
        let mut text = String::from(message);
        for (n, (key, value)) in key_values.enumerate() {
            let _ = write!(text, "{}{key}={value}", if n == 0 { "; " } else { ", " });
        }
        log::logger().log(
            &log::RecordBuilder::new()
                .level(level)
                .target(target)
                .args(format_args!("{text}"))
                .build(),
        )
    }

    fn client_name(&self) -> Cow<'_, str> {
        Cow::Borrowed(env!("CARGO_PKG_NAME"))
    }

    fn client_version(&self) -> Cow<'_, str> {
        Cow::Borrowed(env!("CARGO_PKG_VERSION"))
    }

    fn supports_connection_type(&self, connection_type: ConnectionType) -> bool {
        matches!(
            connection_type,
            ConnectionType::TcpIpv4
                | ConnectionType::TcpIpv6
                | ConnectionType::TcpDns
                | ConnectionType::WebSocketIpv4 { .. }
                | ConnectionType::WebSocketIpv6 { .. }
                | ConnectionType::WebSocketDns { .. }
        )
    }

    fn connect_stream(&self, address: Address) -> Self::StreamConnectFuture {
        // (TCP target, WebSocket `Host` header, TLS server name)
        let (target, ws_host, tls_name): (Result<SocketAddr, (String, u16)>, Option<String>, Option<String>) =
            match address {
                Address::TcpDns { hostname, port } => (Err((hostname.into(), port)), None, None),
                Address::TcpIp { ip, port } => (Ok(SocketAddr::from((ip, port))), None, None),
                Address::WebSocketDns { hostname, port, secure } => (
                    Err((hostname.into(), port)),
                    Some(format!("{hostname}:{port}")),
                    secure.then(|| hostname.to_owned()),
                ),
                Address::WebSocketIp { ip, port } => {
                    let addr = SocketAddr::from((ip, port));
                    (Ok(addr), Some(addr.to_string()), None)
                }
                #[allow(unreachable_patterns)]
                _ => unreachable!(),
            };
        let tls = self.0.tls.0.clone();
        let socket = async move {
            let tcp = match target {
                Ok(addr) => smol::net::TcpStream::connect(addr).await?,
                Err((host, port)) => smol::net::TcpStream::connect((&host[..], port)).await?,
            };
            let _ = tcp.set_nodelay(true);
            let Some(host) = ws_host else {
                return Ok(Box::new(tcp) as Socket);
            };
            let Some(name) = tls_name else {
                let config = websocket::Config { tcp_socket: tcp, host: &host, url: "/" };
                return Ok(Box::new(websocket::websocket_client_handshake(config).await?) as Socket);
            };
            let name = ServerName::try_from(name).map_err(io::Error::other)?;
            let tcp_socket = tls.connect(name, tcp).await?;
            let config = websocket::Config { tcp_socket, host: &host, url: "/" };
            Ok(Box::new(websocket::websocket_client_handshake(config).await?) as Socket)
        };
        future::ready(Stream(with_buffers::WithBuffers::new(Box::pin(socket))))
    }

    fn connect_multistream(&self, _: MultiStreamAddress) -> Self::MultiStreamConnectFuture {
        unreachable!()
    }

    fn open_out_substream(&self, c: &mut Self::MultiStream) {
        match *c {}
    }

    fn next_substream(&self, c: &mut Self::MultiStream) -> Self::NextSubstreamFuture<'_> {
        match *c {}
    }

    fn read_write_access<'a>(
        &self,
        stream: Pin<&'a mut Stream>,
    ) -> Result<Self::ReadWriteAccess<'a>, &'a io::Error> {
        stream.project().0.read_write_access(Instant::now())
    }

    fn wait_read_write_again<'a>(&self, stream: Pin<&'a mut Stream>) -> Self::StreamUpdateFuture<'a> {
        Box::pin(stream.project().0.wait_read_write_again(|when| async move {
            smol::Timer::at(when).await;
        }))
    }
}

