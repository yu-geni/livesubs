//! Waiting for a socket to have something to read.
use std::{io, net::TcpStream, os::windows::io::AsRawSocket, time::Duration};
use windows::Win32::Networking::WinSock::{WSAGetLastError, WSAPoll, POLLRDNORM, SOCKET, WSAPOLLFD, WSAPOLL_EVENT_FLAGS};

/// True when the socket has something to read (data, the peer's close, or an error that the
/// read will report), false when `wait` went by without. The sockets of this app are polled
/// this way rather than with a timeout on the read itself (SO_RCVTIMEO): Windows leaves a
/// socket whose read timed out in an undefined state that "should not be used", and a read on
/// it now and then failed with error 997, which cut the Chrome extension's connection every
/// few minutes.
pub fn readable(s: &TcpStream, wait: Duration) -> io::Result<bool> {
    let mut fd = WSAPOLLFD { fd: SOCKET(s.as_raw_socket() as usize), events: POLLRDNORM, revents: WSAPOLL_EVENT_FLAGS(0) };
    let ms = wait.as_millis().min(i32::MAX as u128) as i32;
    let n = unsafe { WSAPoll(&mut fd, 1, ms) };
    if n < 0 {
        return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }.0));
    }
    Ok(n > 0)
}
