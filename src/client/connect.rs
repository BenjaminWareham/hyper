use net2::TcpBuilder;
use std::error::Error as StdError;
use std::fmt;
use std::io;
use std::mem;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use futures::future::{ExecuteError, Executor};
use futures::sync::oneshot;
use futures::{Async, Future, Poll};
use futures_cpupool::Builder as CpuPoolBuilder;
use tokio::net::TcpStream;
use tokio::reactor::Handle;
use tokio_io::{AsyncRead, AsyncWrite};
use tokio_service::Service;
use Uri;

use super::dns;

/// A connector creates an Io to a remote address..
///
/// This trait is not implemented directly, and only exists to make
/// the intent clearer. A connector should implement `Service` with
/// `Request=Uri` and `Response: Io` instead.
pub trait Connect: Service<Request = Uri, Error = io::Error> + 'static {
    /// The connected Io Stream.
    type Output: AsyncRead + AsyncWrite + 'static;
    /// A Future that will resolve to the connected Stream.
    type Future: Future<Item = Self::Output, Error = io::Error> + 'static;
    /// Connect to a remote address.
    fn connect(&self, Uri) -> <Self as Connect>::Future;
}

impl<T> Connect for T
where
    T: Service<Request = Uri, Error = io::Error> + 'static,
    T::Response: AsyncRead + AsyncWrite,
    T::Future: Future<Error = io::Error>,
{
    type Output = T::Response;
    type Future = T::Future;

    fn connect(&self, url: Uri) -> <Self as Connect>::Future {
        self.call(url)
    }
}

/// A connector for the `http` scheme.
#[derive(Clone)]
pub struct HttpConnector {
    executor: HttpConnectExecutor,
    enforce_http: bool,
    handle: Handle,
    keep_alive_timeout: Option<Duration>,
    local_address: Option<IpAddr>,
}

impl HttpConnector {
    /// Construct a new HttpConnector.
    ///
    /// Takes number of DNS worker threads.
    #[inline]
    pub fn new(threads: usize, handle: &Handle) -> HttpConnector {
        let pool = CpuPoolBuilder::new()
            .name_prefix("hyper-dns")
            .pool_size(threads)
            .create();
        HttpConnector::new_with_executor(pool, handle)
    }

    /// Construct a new HttpConnector.
    ///
    /// Takes an executor to run blocking tasks on.
    #[inline]
    pub fn new_with_executor<E: 'static>(executor: E, handle: &Handle) -> HttpConnector
    where
        E: Executor<HttpConnectorBlockingTask>,
    {
        HttpConnector {
            executor: HttpConnectExecutor(Arc::new(executor)),
            enforce_http: true,
            handle: handle.clone(),
            keep_alive_timeout: None,
            local_address: None,
        }
    }

    /// Option to enforce all `Uri`s have the `http` scheme.
    ///
    /// Enabled by default.
    #[inline]
    pub fn enforce_http(&mut self, is_enforced: bool) {
        self.enforce_http = is_enforced;
    }

    /// Set that all sockets have `SO_KEEPALIVE` set with the supplied duration.
    ///
    /// If `None`, the option will not be set.
    ///
    /// Default is `None`.
    #[inline]
    pub fn set_keepalive(&mut self, dur: Option<Duration>) {
        self.keep_alive_timeout = dur;
    }

    /// Set that all sockets are optionally bound to the configured address.
    ///
    /// If `None`, the option will not be set.
    ///
    /// Default is `None`.
    #[inline]
    pub fn set_local_address(&mut self, addr: Option<IpAddr>) {
        self.local_address = addr;
    }
}

impl fmt::Debug for HttpConnector {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("HttpConnector").finish()
    }
}

impl Service for HttpConnector {
    type Request = Uri;
    type Response = TcpStream;
    type Error = io::Error;
    type Future = HttpConnecting;

    fn call(&self, uri: Uri) -> Self::Future {
        trace!("Http::connect({:?})", uri);

        if self.enforce_http {
            if uri.scheme() != Some("http") {
                return invalid_url(InvalidUrl::NotHttp, &self.handle);
            }
        } else if uri.scheme().is_none() {
            return invalid_url(InvalidUrl::MissingScheme, &self.handle);
        }

        let host = match uri.host() {
            Some(s) => s,
            None => return invalid_url(InvalidUrl::MissingAuthority, &self.handle),
        };
        let port = match uri.port() {
            Some(port) => port,
            None => match uri.scheme() {
                Some("https") => 443,
                _ => 80,
            },
        };

        HttpConnecting {
            state: State::Lazy(self.executor.clone(), host.into(), port, self.local_address),
            handle: self.handle.clone(),
            keep_alive_timeout: self.keep_alive_timeout,
        }
    }
}

#[inline]
fn invalid_url(err: InvalidUrl, handle: &Handle) -> HttpConnecting {
    HttpConnecting {
        state: State::Error(Some(io::Error::new(io::ErrorKind::InvalidInput, err))),
        handle: handle.clone(),
        keep_alive_timeout: None,
    }
}

#[derive(Debug, Clone, Copy)]
enum InvalidUrl {
    MissingScheme,
    NotHttp,
    MissingAuthority,
}

impl fmt::Display for InvalidUrl {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(self.description())
    }
}

impl StdError for InvalidUrl {
    fn description(&self) -> &str {
        match *self {
            InvalidUrl::MissingScheme => "invalid URL, missing scheme",
            InvalidUrl::NotHttp => "invalid URL, scheme must be http",
            InvalidUrl::MissingAuthority => "invalid URL, missing domain",
        }
    }
}

/// A Future representing work to connect to a URL.
#[must_use = "futures do nothing unless polled"]
pub struct HttpConnecting {
    state: State,
    handle: Handle,
    keep_alive_timeout: Option<Duration>,
}

enum State {
    Lazy(HttpConnectExecutor, String, u16, Option<IpAddr>),
    Resolving(
        oneshot::SpawnHandle<dns::IpAddrs, io::Error>,
        Option<IpAddr>,
    ),
    Connecting(ConnectingTcp),
    Error(Option<io::Error>),
}

impl Future for HttpConnecting {
    type Item = TcpStream;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let state;
            match self.state {
                State::Lazy(ref executor, ref mut host, port, local_address) => {
                    // If the host is already an IP addr (v4 or v6),
                    // skip resolving the dns and start connecting right away.
                    if let Some(addrs) = dns::IpAddrs::try_parse(host, port) {
                        state = State::Connecting(ConnectingTcp {
                            local_address: local_address,
                            addrs: addrs,
                            current: None,
                        })
                    } else {
                        let host = mem::replace(host, String::new());
                        let work = dns::Work::new(host, port);
                        state = State::Resolving(oneshot::spawn(work, executor), local_address);
                    }
                }
                State::Resolving(ref mut future, local_address) => {
                    match try!(future.poll()) {
                        Async::NotReady => return Ok(Async::NotReady),
                        Async::Ready(addrs) => {
                            state = State::Connecting(ConnectingTcp {
                                local_address: local_address,
                                addrs: addrs,
                                current: None,
                            })
                        }
                    };
                }
                State::Connecting(ref mut c) => {
                    let sock = try_ready!(c.poll(&self.handle));

                    if let Some(dur) = self.keep_alive_timeout {
                        sock.set_keepalive(Some(dur))?;
                    }

                    return Ok(Async::Ready(sock));
                }
                State::Error(ref mut e) => return Err(e.take().expect("polled more than once")),
            }
            self.state = state;
        }
    }
}

impl fmt::Debug for HttpConnecting {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.pad("HttpConnecting")
    }
}

struct ConnectingTcp {
    local_address: Option<IpAddr>,
    addrs: dns::IpAddrs,
    current: Option<Box<Future<Item = TcpStream, Error = io::Error>>>,
}

// Connect to the given TCP address, optionally binding the local address.
fn tcp_connect(
    addr: &SocketAddr,
    local: &Option<IpAddr>,
    handle: &Handle,
) -> Result<Box<Future<Item = TcpStream, Error = io::Error> + Send>, io::Error> {
    let do_bind = |local: &Option<IpAddr>| {
        let sock = match addr {
            &SocketAddr::V4(..) => TcpBuilder::new_v4(),
            &SocketAddr::V6(..) => TcpBuilder::new_v6(),
        }?;
        match local {
            &None => (), // missing special Windows knowledge
            &Some(ref local_addr) => sock
                .bind(SocketAddr::new(local_addr.clone(), 0))
                .map(|_| ())?,
        };
        sock.to_tcp_stream()
    };

    do_bind(local).map(|tcp| TcpStream::connect_stream(tcp, addr, handle))
}

impl ConnectingTcp {
    // not a Future, since passing a &Handle to poll
    fn poll(&mut self, handle: &Handle) -> Poll<TcpStream, io::Error> {
        let mut err = None;
        loop {
            if let Some(ref mut current) = self.current {
                debug!("if section of poll");
                match current.poll() {
                    Ok(ok) => return Ok(ok),
                    Err(e) => {
                        trace!("connect error {:?}", e);
                        err = Some(e);
                        // SFR 528468 - Try all returned records
                        for addr in self.addrs.clone() {
                            // SFR 529157 - match local to remote IP version
                            if let Some(local_addr) = self.local_address {
                                if addr.is_ipv4() != local_addr.is_ipv4() {
                                    continue;
                                }
                            }
                            debug!("connecting to {}", addr);
                            match tcp_connect(&addr, &self.local_address, handle) {
                                Ok(stream) => {
                                    *current = stream;
                                    break;
                                }
                                Err(e) => {
                                    err = Some(e);
                                    debug!("hit an error connecting to {}: {:?}", addr, err)
                                    // fall through and report error
                                }
                            }
                        }
                    }
                }
            } else {
                // SFR 528468 - Try all returned records
                debug!("else section of poll");
                for addr in self.addrs.clone() {
                    // SFR 529157 - match local to remote IP version
                    if let Some(local_addr) = self.local_address {
                        if addr.is_ipv4() != local_addr.is_ipv4() {
                            continue;
                        }
                    }
                    debug!("connecting to {}", addr);
                    match tcp_connect(&addr, &self.local_address, handle) {
                        Ok(stream) => {
                            self.current = Some(stream);
                            break;
                        }
                        Err(e) => {
                            err = Some(e);
                            debug!("hit an error connecting to {}: {:?}", addr, err)
                            // fall through and report error
                        }
                    };
                }
            }

            return Err(err.take().expect("missing connect error"));
        }
    }
}

/// Blocking task to be executed on a thread pool.
pub struct HttpConnectorBlockingTask {
    work: oneshot::Execute<dns::Work>,
}

impl fmt::Debug for HttpConnectorBlockingTask {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.pad("HttpConnectorBlockingTask")
    }
}

impl Future for HttpConnectorBlockingTask {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<(), ()> {
        self.work.poll()
    }
}

#[derive(Clone)]
struct HttpConnectExecutor(Arc<Executor<HttpConnectorBlockingTask>>);

impl Executor<oneshot::Execute<dns::Work>> for HttpConnectExecutor {
    fn execute(
        &self,
        future: oneshot::Execute<dns::Work>,
    ) -> Result<(), ExecuteError<oneshot::Execute<dns::Work>>> {
        self.0
            .execute(HttpConnectorBlockingTask { work: future })
            .map_err(|err| ExecuteError::new(err.kind(), err.into_future().work))
    }
}

#[cfg(test)]
mod tests {
    use super::{Connect, HttpConnector};
    use std::io;
    use tokio::reactor::Core;

    #[test]
    fn test_errors_missing_authority() {
        let mut core = Core::new().unwrap();
        let url = "/foo/bar?baz".parse().unwrap();
        let connector = HttpConnector::new(1, &core.handle());

        assert_eq!(
            core.run(connector.connect(url)).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn test_errors_enforce_http() {
        let mut core = Core::new().unwrap();
        let url = "https://example.domain/foo/bar?baz".parse().unwrap();
        let connector = HttpConnector::new(1, &core.handle());

        assert_eq!(
            core.run(connector.connect(url)).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn test_errors_missing_scheme() {
        let mut core = Core::new().unwrap();
        let url = "example.domain".parse().unwrap();
        let connector = HttpConnector::new(1, &core.handle());

        assert_eq!(
            core.run(connector.connect(url)).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
