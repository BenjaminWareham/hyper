//#![deny(warnings)]
extern crate futures;
extern crate hyper;
extern crate tokio_core;

extern crate pretty_env_logger;

use std::env;
use std::io::{self, Write};
use std::net::IpAddr;

use futures::Future;
use futures::stream::Stream;

use hyper::Client;
use hyper::client::HttpConnector;

fn main() {
    pretty_env_logger::init();

    let url = match env::args().nth(1) {
        Some(url) => url,
        None => {
            println!("Usage: client <url> [<bind_addr>]");
            return;
        }
    };

    let bind_addr = env::args().nth(2);

    let bind_addr: Option<IpAddr> = bind_addr.map(|s| s.parse::<IpAddr>().unwrap());

    let url = url.parse::<hyper::Uri>().unwrap();
    if url.scheme() != Some("http") {
        println!("This example only works with 'http' URLs.");
        return;
    }

    let mut core = tokio_core::reactor::Core::new().unwrap();
    let handle = core.handle();
    let mut connector = HttpConnector::new(4, &handle);
    connector.set_local_address(bind_addr);
    let client = Client::configure().connector(connector).build(&handle);

    let work = client.get(url).and_then(|res| {
        println!("Response: {}", res.status());
        println!("Headers: \n{}", res.headers());

        res.body().for_each(|chunk| {
            io::stdout().write_all(&chunk).map_err(From::from)
        })
    }).map(|_| {
        println!("\n\nDone.");
    });

    core.run(work).unwrap();
}
