use std::io;
use std::net::IpAddr;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6, ToSocketAddrs};
use std::vec;

use futures::{Async, Future, Poll};

pub struct Work {
    host: String,
    port: u16,
}

impl Work {
    pub fn new(host: String, port: u16) -> Work {
        Work {
            host: host,
            port: port,
        }
    }
}

impl Future for Work {
    type Item = IpAddrs;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        debug!("resolving host={:?}, port={:?}", self.host, self.port);
        (&*self.host, self.port)
            .to_socket_addrs()
            .map(|i| Async::Ready(IpAddrs { iter: i }))
    }
}

#[derive(Clone)]
pub struct IpAddrs {
    iter: vec::IntoIter<SocketAddr>,
}

impl IpAddrs {
    pub fn try_parse(host: &str, port: u16) -> Option<IpAddrs> {
        if let Ok(addr) = host.parse::<Ipv4Addr>() {
            let addr = SocketAddrV4::new(addr, port);
            return Some(IpAddrs {
                iter: vec![SocketAddr::V4(addr)].into_iter(),
            });
        }
        if let Ok(addr) = host.parse::<Ipv6Addr>() {
            let addr = SocketAddrV6::new(addr, port, 0, 0);
            return Some(IpAddrs {
                iter: vec![SocketAddr::V6(addr)].into_iter(),
            });
        }
        None
    }
    pub fn next_filter(&mut self, local_addr: Option<IpAddr>) -> Option<SocketAddr> {
        if let Some(ip_addr) = local_addr {
            self.iter.find(|addr| addr.is_ipv4() == ip_addr.is_ipv4())
        } else {
            self.iter.next()
        }
    }
}

impl Iterator for IpAddrs {
    type Item = SocketAddr;
    #[inline]
    fn next(&mut self) -> Option<SocketAddr> {
        self.iter.next()
    }
}
