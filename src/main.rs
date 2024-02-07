pub mod daemon;
pub mod event;
pub mod journal;

use self::daemon::*;
use self::event::*;
use self::journal::*;
use anyhow::Result;
use std::mem;
use std::net;
use std::pin::*;

fn main() -> Result<std::process::ExitCode> {
    let args: Args = clap::Parser::parse();

    log_debug!("Starting up...");

    let mut event = Event::default();
    let mut pinned = pin!(event);

    // setting up process signal handling for graceful shutdown
    pinned.as_mut().add_signal_default(Signal::TERM);
    pinned.as_mut().add_signal(Signal::INT, |source, _| {
        log_warning!("How dare you interrupt me!");
        source.event().exit(240);
    });

    // setting up ActivityMonitor for monitoring activity and shutting down the service when there
    // is no more activity (which is then used by systemd PropagatesStopTo= directive to shut down
    // the server we are proxying to
    struct ActivityMonitor {
        id: EventSourceId,
        usec: u64,
    }

    impl ActivityMonitor {
        fn ping(&mut self, event: Pin<&mut Event>) {
            if let Some(mut source) = event.get_event_source_time(self.id) {
                let now = source.event().now(Clock::Monotonic);
                source.set_time(Usec::Absolute(now + self.usec));
            }
        }
    }

    *pinned.as_mut().userdata() = args.exit_idle_time.map(|seconds| {
        let usec = seconds * 1000000;
        let now = pinned.now(Clock::Monotonic);
        let mut source = pinned.as_mut().add_time(
            Clock::Monotonic,
            Usec::Absolute(now + usec),
            0,
            move |source, _now| {
                log_warning!("No activity detected for a while, exiting...");
                source.event().exit(0);
                Usec::Absolute(0)
            },
        );
        source.enable();
        ActivityMonitor {
            id: source.id(),
            usec,
        }
    });

    // going through all the sockets received by systemd and all the server addresses received in
    // arguments and opening proxies for each and every one of them
    let network_timeout = args.timeout * 1000000;
    for (object, addr_out) in listen_fds(false).into_iter().zip(args.addr_out) {
        match object {
            ListenObject::UdpSocket(socket_in) => {
                log_info!(
                    " * UDP: {} <-> {}",
                    socket_in.local_addr().unwrap(),
                    addr_out
                );
                let addr_out: net::SocketAddr = addr_out.parse()?;

                // a buffer that holds the data, the length of the data and the source origin
                struct Buffer {
                    buffer: [u8; 65507],
                    n: usize,
                    src: net::SocketAddr,
                }

                impl Default for Buffer {
                    fn default() -> Self {
                        Self {
                            buffer: [0; 65507],
                            n: 0,
                            src: net::SocketAddr::V4(net::SocketAddrV4::new(
                                net::Ipv4Addr::UNSPECIFIED,
                                0,
                            )),
                        }
                    }
                }

                impl Buffer {
                    // take the slice of this buffer and resets its data
                    #[inline]
                    fn take_slice(&mut self) -> Option<(&[u8], net::SocketAddr)> {
                        if self.n > 0 {
                            let slice = &self.buffer[..self.n];
                            self.n = 0;
                            Some((slice, self.src))
                        } else {
                            None
                        }
                    }

                    // get a mutable slice of this buffer only if no data is being hold
                    #[inline]
                    fn get_slice_mut(
                        &mut self,
                    ) -> Option<(&mut [u8], &mut usize, &mut net::SocketAddr)> {
                        if self.n > 0 {
                            None
                        } else {
                            Some((&mut self.buffer, &mut self.n, &mut self.src))
                        }
                    }

                    // explicitly reset this buffer, allowing it to be filled again
                    #[inline]
                    fn reset(&mut self) {
                        self.n = 0;
                    }
                }

                struct Client {
                    id: EventSourceId,
                    addr: Option<net::SocketAddr>,
                    buffer_in: Buffer,
                    buffer_out: Buffer,
                    last_activity: u64,
                }

                struct State {
                    clients: Vec<Client>,
                    buffer_in: Buffer,
                }

                impl Default for State {
                    fn default() -> Self {
                        Self {
                            clients: Vec::new(),
                            buffer_in: Default::default(),
                        }
                    }
                }

                // a function that finds a valid Client to use for a specific client's IP address
                //
                // either the client's IP is already registered, or it will return an empty slot
                // that can be used
                fn find_client(
                    clients: &[Client],
                    addr: net::SocketAddr,
                    timeout_threshold: u64,
                ) -> Option<usize> {
                    let mut slot = None;

                    for (i, client) in clients.iter().enumerate() {
                        match client.addr {
                            Some(x) if x == addr => {
                                slot.replace(i);
                                break;
                            }
                            None if slot.is_none() => {
                                slot.replace(i);
                            }
                            _ => {}
                        }

                        if client.last_activity < timeout_threshold && slot.is_none() {
                            slot.replace(i);
                        }
                    }

                    slot
                }

                let socket_in_id = pinned
                    .as_mut()
                    .add_io(
                        socket_in,
                        Events::EPOLLIN,
                        move |mut source, socket, mut events| {
                            if let Some(activity_monitor) =
                                source.event().userdata::<Option<ActivityMonitor>>()
                            {
                                activity_monitor.ping(source.event());
                            }

                            events
                                .handle(Events::EPOLLIN, || {
                                    let State {
                                        clients, buffer_in, ..
                                    } = source.event().userdata::<State>();

                                    if let Some((buffer, n, src)) = buffer_in.get_slice_mut() {
                                        (*n, *src) = socket.recv_from(buffer).unwrap();
                                        let now = source.event().now(Clock::Monotonic);
                                        let timeout_threshold = now - network_timeout;
                                        if let Some(i) =
                                            find_client(clients, *src, timeout_threshold)
                                        {
                                            let client = &mut clients[i];
                                            if client.addr.is_none()
                                                || client.last_activity < timeout_threshold
                                            {
                                                log_debug!("New client connected ({i}): {src}");
                                            }
                                            client.addr.replace(*src);
                                            client.last_activity = now;
                                            mem::swap(&mut client.buffer_out, buffer_in);
                                            if let Some(mut source) =
                                                source.event().get_event_source_io(client.id)
                                            {
                                                source.add_events(Events::EPOLLOUT);
                                            }
                                        } else {
                                            log_warning!("Maximum number of connections reached.");
                                        }
                                        buffer_in.reset();
                                    } else {
                                        source.remove_events(Events::EPOLLIN);
                                    }
                                })
                                .handle(Events::EPOLLOUT, || {
                                    source.remove_events(Events::EPOLLOUT);

                                    let State { clients, .. } = source.event().userdata::<State>();

                                    for client in clients {
                                        if let Some(addr) = client.addr {
                                            if let Some((buffer, _src)) =
                                                client.buffer_in.take_slice()
                                            {
                                                let _ = socket.send_to(buffer, addr);
                                            }
                                        }
                                    }
                                })
                                .end()
                        },
                    )
                    .id();

                pinned.as_mut().userdata::<State>().clients = (0..args.connections_max)
                    .map(|i| -> Result<Client> {
                        let socket = net::UdpSocket::bind((net::Ipv4Addr::UNSPECIFIED, 0))?;
                        socket.connect(addr_out)?;
                        let id = pinned
                            .as_mut()
                            .add_io(
                                socket,
                                Events::EPOLLIN,
                                move |mut source, socket, mut events| {
                                    events
                                        .handle(Events::EPOLLIN, || {
                                            let State { clients, .. } =
                                                source.event().userdata::<State>();

                                            if let Some((buf, n, src)) =
                                                clients[i].buffer_in.get_slice_mut()
                                            {
                                                (*n, *src) = socket.recv_from(buf).unwrap();

                                                if let Some(mut source) =
                                                    source.event().get_event_source_io(socket_in_id)
                                                {
                                                    source.add_events(Events::EPOLLOUT);
                                                }
                                            }
                                        })
                                        .handle(Events::EPOLLOUT, || {
                                            source.remove_events(Events::EPOLLOUT);

                                            let State { clients, .. } =
                                                source.event().userdata::<State>();
                                            let buffer = &mut clients[i].buffer_out;

                                            if let Some((buffer, _src)) = buffer.take_slice() {
                                                let _ = socket.send_to(buffer, addr_out);
                                            }
                                        })
                                        .end()
                                },
                            )
                            .id();

                        Ok(Client {
                            id,
                            addr: None,
                            buffer_in: Buffer::default(),
                            buffer_out: Buffer::default(),
                            last_activity: 0,
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
            }
            ListenObject::TcpListener(socket_in) => {
                use pipe::Pipe;
                use std::io::Read;

                log_info!(
                    " * TCP: {} <-> {}",
                    socket_in.local_addr().unwrap(),
                    addr_out
                );
                let addr_out: net::SocketAddr = addr_out.parse()?;
                let connections_max = args.connections_max;
                let mut buffer = [0; 65507];

                struct Client {
                    source_in: EventSourceId,
                    source_out: EventSourceId,
                    pipe_in: Pipe,
                    pipe_out: Pipe,
                    closed: bool,
                }

                #[derive(Default)]
                struct State {
                    clients: Vec<Client>,
                }

                pinned.as_mut().add_io(
                    socket_in,
                    Events::EPOLLIN,
                    move |mut source, socket, mut events| {
                        events
                            .handle(Events::EPOLLIN, move || {
                                use std::os::fd::AsRawFd;
                                dbg!(socket.as_raw_fd());

                                let (mut stream_in, src) = socket.accept().unwrap();
                                dbg!(stream_in.as_raw_fd());

                                let n = stream_in.read(&mut buffer).unwrap();
                                let s = String::from_utf8_lossy(&buffer[..n]);
                                dbg!(src, s);

                                let stream_out = net::TcpStream::connect(addr_out).unwrap();

                                let State { clients, .. } = source.event().userdata::<State>();

                                if clients.iter().filter(|x| !x.closed).count() < connections_max {
                                    let (Ok(pipe_in), Ok(pipe_out)) = (Pipe::new(), Pipe::new())
                                    else {
                                        log_err!("Could not create new pipes.");
                                        return;
                                    };

                                    let id = source.id();
                                    {
                                    let mut source =
                                        source.event().get_event_source_io(id).unwrap();
                                    source.drop();
                                    }
                                    source.get_events();

                                    /*
                                    clients.push(Client {
                                        closed: false,
                                        pipe_in,
                                        pipe_out,
                                    });
                                    */
                                }
                            })
                            .handle(Events::EPOLLHUP, || {
                                dbg!("client hangs up");
                            })
                            .end()
                    },
                );
            }
        }
    }

    notify_ready();

    let res = pinned.run();

    log_debug!("Event log exited gracefully.");

    Ok((res as u8).into())
}

#[derive(Debug, clap::Parser)]
struct Args {
    /// Target sockets.
    #[clap(id = " HOST:PORT | SOCKET ")]
    addr_out: Vec<String>,

    /// Maximum number of clients connected (per socket).
    #[arg(long, short = 'c', default_value = "32")]
    connections_max: usize,

    /// Connection timeout delay (in seconds).
    #[arg(long, default_value = "30")]
    timeout: u64,

    /// Exit when without a connection for this duration (in seconds).
    #[arg(long)]
    exit_idle_time: Option<u64>,
}

mod pipe {
    use std::io;
    use std::os::fd::*;
    use std::ptr;

    pub struct Pipe {
        fd_r: OwnedFd,
        fd_w: OwnedFd,
        size: usize,
    }

    impl Pipe {
        pub fn new() -> io::Result<Self> {
            unsafe {
                let mut fds: [libc::c_int; 2] = [0; 2];
                let res = libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK);
                if res < 0 {
                    return Err(io::Error::last_os_error());
                }
                libc::fcntl(fds[0], libc::F_SETPIPE_SZ, 256 * 1024);
                let res = libc::fcntl(fds[0], libc::F_GETPIPE_SZ);
                if res < 0 {
                    return Err(io::Error::last_os_error());
                }
                let size = res as usize;
                Ok(Self {
                    fd_r: OwnedFd::from_raw_fd(fds[0]),
                    fd_w: OwnedFd::from_raw_fd(fds[1]),
                    size,
                })
            }
        }

        // NOTE: this code is mostly copied from socket-proxyd.c
        pub fn shovel(&mut self, from: impl AsRawFd, to: impl AsRawFd, full: &mut usize) {
            let from = from.as_raw_fd();
            let to = to.as_raw_fd();

            unsafe {
                let mut shoveled;
                loop {
                    shoveled = false;

                    if *full < self.size {
                        let res = libc::splice(
                            from,
                            ptr::null_mut(),
                            self.fd_w.as_raw_fd(),
                            ptr::null_mut(),
                            self.size - *full,
                            libc::SPLICE_F_MOVE | libc::SPLICE_F_NONBLOCK,
                        );
                        assert!(res >= 0, "Failed to splice");
                        if res > 0 {
                            *full += res as usize;
                            shoveled = true;
                        } else if res == 0 {
                            // EOF
                        }
                    }

                    if *full > 0 {
                        let res = libc::splice(
                            self.fd_r.as_raw_fd(),
                            ptr::null_mut(),
                            to,
                            ptr::null_mut(),
                            *full,
                            libc::SPLICE_F_MOVE | libc::SPLICE_F_NONBLOCK,
                        );
                        assert!(res >= 0, "Failed to splice");
                        if res > 0 {
                            *full -= res as usize;
                            shoveled = true;
                        } else if res == 0 {
                            // EOF
                        }
                    }

                    if !shoveled {
                        break;
                    }
                }
            }
        }
    }
}
