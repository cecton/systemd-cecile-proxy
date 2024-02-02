use libsystemd_sys::daemon as ffi;
use std::ffi::*;
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

pub fn notify(unset_environment: bool, state: impl AsRef<str>) -> Result<(), NulError> {
    unsafe {
        let s = CString::new(state.as_ref())?;
        assert!(
            ffi::sd_notify(if unset_environment { 1 } else { 0 }, s.as_ptr()) >= 0,
            "failed to notify"
        );
        Ok(())
    }
}

#[inline]
pub fn notify_ready() {
    let _ = notify(false, "READY=1");
}

#[inline]
pub fn notify_reloading() {
    let _ = notify(false, "RELOADING=1");
}

#[inline]
pub fn notify_stopping() {
    let _ = notify(false, "STOPPING=1");
}

#[inline]
pub fn notify_monotonic_usec(usec: u64) {
    let _ = notify(false, format!("MONOTONIC_USEC={usec}"));
}

#[inline]
pub fn notify_status(status: impl std::fmt::Display) {
    let _ = notify(false, format!("STATUS={}", status));
}
