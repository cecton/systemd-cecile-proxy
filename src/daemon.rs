use libsystemd_sys::daemon as ffi;
use std::os::fd::*;

const SD_LISTEN_FDS_START: RawFd = 3;

pub fn listen_fds(unset_environment: bool) -> Vec<ListenObject> {
    unsafe {
        let count = ffi::sd_listen_fds(if unset_environment { 1 } else { 0 });
        (SD_LISTEN_FDS_START..(SD_LISTEN_FDS_START + count))
            .map(|fd| {
                match ffi::sd_is_socket(fd, libc::AF_UNSPEC, libc::SOCK_DGRAM, -1) {
                    0 => {}
                    res if res < 0 => panic!("Failed to determine socket type."),
                    _ => {
                        return ListenObject::UdpSocket(std::net::UdpSocket::from_raw_fd(fd));
                    }
                }

                match ffi::sd_is_socket(fd, libc::AF_UNSPEC, libc::SOCK_STREAM, -1) {
                    0 => {}
                    res if res < 0 => panic!("Failed to determine socket type."),
                    _ => {
                        return ListenObject::TcpListener(std::net::TcpListener::from_raw_fd(fd));
                    }
                }

                todo!("file descriptor not recognized")
            })
            .collect()
    }
}

#[derive(Debug)]
pub enum ListenObject {
    UdpSocket(std::net::UdpSocket),
    TcpListener(std::net::TcpListener),
}

impl AsRawFd for ListenObject {
    fn as_raw_fd(&self) -> i32 {
        match self {
            ListenObject::UdpSocket(socket) => socket.as_raw_fd(),
            ListenObject::TcpListener(socket) => socket.as_raw_fd(),
        }
    }
}
