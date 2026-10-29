//! An address that refuses connections, for tests of transport failures.

use std::net::SocketAddr;

use tokio::net::TcpSocket;

/// A loopback address that refuses every connection while the returned
/// socket is held. The socket is bound but never listens, so the port
/// stays reserved: a port freed for the test could be taken by another
/// test's server and answer.
pub(crate) fn refusing() -> (TcpSocket, SocketAddr) {
    let socket = TcpSocket::new_v4().unwrap();
    socket.bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
    let address = socket.local_addr().unwrap();
    (socket, address)
}
