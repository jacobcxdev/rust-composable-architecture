//! Native loopback connections with bounded I/O for ownership tests.

use std::{
    io::Read,
    net::{Ipv4Addr, TcpListener, TcpStream},
    time::Duration,
};

/// Opens two local endpoints. The timeout bounds stalled I/O, not release latency.
pub(super) fn pair() -> (TcpStream, TcpStream) {
    let limit = Duration::from_secs(3);
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let peer = TcpStream::connect_timeout(&listener.local_addr().unwrap(), limit).unwrap();
    let (owner, _) = listener.accept().unwrap();
    for stream in [&owner, &peer] {
        stream.set_read_timeout(Some(limit)).unwrap();
        stream.set_write_timeout(Some(limit)).unwrap();
    }
    (owner, peer)
}

/// Requires an empty read on a nonempty buffer. A timeout is not EOF.
pub(super) fn assert_eof(peer: &mut TcpStream) {
    assert_eq!(peer.read(&mut [0]).expect("read from the native peer"), 0);
}
