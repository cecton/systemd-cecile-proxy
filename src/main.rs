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

    log_info!("Starting up...");

    let mut event = Event::default();
    let mut pinned = event.pinned();

    pinned.as_mut().add_signal_default(Signal::TERM);
    pinned.as_mut().add_signal(Signal::INT, |source, _| {
        log_warning!("How dare you interrupt me!");
        source.event().exit(240);
    });

    struct ActivityMonitor {
        id: EventSourceId,
        usec: u64,
    }

    impl ActivityMonitor {
        fn ping(&mut self, mut event: Pin<&mut Event>) {
            if let Some(mut source) = event.get_event_source_time(self.id) {
                let now = source.event().now(Clock::Monotonic);
                source.set_time(Usec::Absolute(now + self.usec));
            }
        }
    }

    *pinned.as_mut().userdata() = args.exit_idle_time.map(|seconds| {
        let usec = seconds * 1000000;
        let now = pinned.now(Clock::Monotonic);
        let id = pinned.as_mut().add_time(
            Clock::Monotonic,
            Usec::Absolute(now + usec),
            0,
            move |source, _now| {
                log_warning!("No activity detected for a while, exiting...");
                source.event().exit(0);
                Usec::Absolute(0)
            },
        );
        pinned.as_mut().enable(id);
        ActivityMonitor { id, usec }
    });

    for (object, addr_out) in listen_fds(false).into_iter().zip(args.addr_out) {
        match object {
            ListenObject::UdpSocket(socket_in) => {
                log_info!(
                    " * UDP: {} <-> {}",
                    socket_in.local_addr().unwrap(),
                    addr_out
                );
                let addr_out: net::SocketAddr = addr_out.parse()?;
                let timeout = args.timeout * 1000000;

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

                fn find_client(
                    clients: &mut [Client],
                    addr: net::SocketAddr,
                    timeout_threshold: u64,
                ) -> Option<&mut Client> {
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

                    slot.map(|i| {
                        let old_addr = clients[i].addr.replace(addr);
                        if old_addr.is_none() || clients[i].last_activity < timeout_threshold {
                            log_debug!("New client connected ({i}): {addr}");
                        }
                        &mut clients[i]
                    })
                }

                let socket_in_id = pinned.as_mut().add_io(
                    socket_in,
                    Events::EPOLLIN,
                    move |source, socket, mut events| {
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
                                    let timeout_threshold = now - timeout;
                                    let src = *src;
                                    if let Some(client) =
                                        find_client(clients, src, timeout_threshold)
                                    {
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
                                        if let Some((buffer, _src)) = client.buffer_in.take_slice()
                                        {
                                            socket.send_to(buffer, addr).unwrap();
                                        }
                                    }
                                }
                            })
                            .end()
                    },
                );

                pinned.as_mut().userdata::<State>().clients = (0..args.connections_max)
                    .map(|i| -> Result<Client> {
                        let socket = net::UdpSocket::bind((net::Ipv4Addr::UNSPECIFIED, 0))?;
                        socket.connect(addr_out)?;
                        let id = pinned.as_mut().add_io(
                            socket,
                            Events::EPOLLIN,
                            move |source, socket, mut events| {
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
                        );

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
                println!("tcp: {} <-> {}", socket_in.local_addr().unwrap(), addr_out);
                let mut buffer = [0; 65507];
                pinned.as_mut().add_io(
                    socket_in,
                    Events::EPOLLIN,
                    move |_source, socket_in, events| {
                        assert!(events & Events::EPOLLIN == Events::EPOLLIN);

                        let (mut stream, src) = socket_in.accept().unwrap();

                        use std::io::Read;
                        let n = stream.read(&mut buffer).unwrap();
                        let s = String::from_utf8_lossy(&buffer[..n]);
                        dbg!(src, s);
                    },
                );
            }
        }
    }

    notify_ready();

    let res = pinned.as_mut().run();

    log_info!("Exiting...");

    Ok((res as u8).into())
}

#[derive(Debug, clap::Parser)]
struct Args {
    /// Target sockets.
    #[clap(id = " HOST:PORT | SOCKET ")]
    addr_out: Vec<String>,

    /// Maximum number of clients.
    #[arg(long, short = 'c', default_value = "32")]
    connections_max: usize,

    /// Connection timeout delay (in seconds).
    #[arg(long, default_value = "30")]
    timeout: u64,

    /// Exit when without a connection for this duration (in seconds).
    #[arg(long)]
    exit_idle_time: Option<u64>,
}
