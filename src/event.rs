use libsystemd_sys::event as ffi;
use std::any::*;
use std::collections::HashMap;
use std::ffi::*;
use std::mem;
use std::num;
use std::os::fd::*;
use std::pin::*;
use std::ptr;

pub struct Event {
    event: *mut ffi::sd_event,
    event_sources: Vec<EventSource>,
    userdata: HashMap<TypeId, Box<dyn Any>>,
    _pin: std::marker::PhantomPinned,
}

impl Default for Event {
    fn default() -> Self {
        unsafe {
            let mut event = ptr::null_mut();
            assert!(
                ffi::sd_event_default(&mut event) >= 0,
                "Cannot instantiate the event loop."
            );
            Self {
                event,
                event_sources: Default::default(),
                userdata: Default::default(),
                _pin: Default::default(),
            }
        }
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        unsafe {
            self.event_sources.clear();
            if !self.event.is_null() {
                assert!(
                    ffi::sd_event_unref(self.event).is_null(),
                    "Cannot destroy event loop."
                );
            }
        }
    }
}

impl Event {
    pub fn new() -> Self {
        Default::default()
    }

    unsafe fn as_mut_ptr(self: Pin<&mut Self>) -> *mut Self {
        self.get_unchecked_mut() as _
    }

    unsafe fn from_mut_ptr<'a>(event: *mut Event) -> Pin<&'a mut Self> {
        let mut ptr = ptr::NonNull::<Event>::new(event).unwrap();
        Pin::new_unchecked(ptr.as_mut())
    }

    unsafe fn with_event_sources<'a, T>(
        self: Pin<&'a mut Self>,
        callback: impl FnOnce(&'a mut Vec<EventSource>) -> T,
    ) -> T {
        callback(&mut self.get_unchecked_mut().event_sources)
    }

    unsafe fn add_event_source(
        self: Pin<&mut Self>,
        event_source: impl Into<EventSource>,
    ) -> &mut EventSource {
        self.with_event_sources(|vec| {
            vec.push(event_source.into());
            vec.last_mut().unwrap()
        })
    }

    unsafe fn remove_event_source(self: Pin<&mut Self>, id: EventSourceId) {
        self.with_event_sources(|vec| {
            if let Some(i) = vec.iter().position(|x| x.id() == id) {
                vec.remove(i);
            }
        })
    }

    // TODO this can be executed from an handler
    pub fn run(self: Pin<&mut Self>) -> i32 {
        unsafe { ffi::sd_event_loop(self.event) }
    }

    pub fn exit(self: Pin<&mut Self>, code: i32) {
        unsafe {
            assert!(
                ffi::sd_event_exit(self.event, code) >= 0,
                "Event loop exit failure."
            );
        }
    }

    pub fn now(&self, clock: Clock) -> u64 {
        unsafe {
            let mut now = 0_u64;
            ffi::sd_event_now(self.event, clock as _, &mut now);
            now
        }
    }

    pub fn add_time(
        mut self: Pin<&mut Self>,
        clock: Clock,
        usec: Usec,
        accuracy: u64,
        callback: impl FnMut(EventSourceHandle<&mut EventSourceTime>, u64) -> Usec + 'static,
    ) -> EventSourceHandle<&mut EventSourceTime> {
        unsafe {
            let event = self.event;
            let mut source = {
                let event = self.as_mut().as_mut_ptr();
                self.add_event_source(EventSourceTime {
                    event_source: ptr::null_mut(),
                    event,
                    handler: Box::new(callback),
                    borrowed: false,
                    _pin: Default::default(),
                })
                .as_time()
                .unwrap()
            };

            unsafe extern "C" fn handler(
                _event_source: *mut ffi::sd_event_source,
                usec: u64,
                userdata: *mut c_void,
            ) -> i32 {
                let Some(mut ptr) = ptr::NonNull::<EventSourceTime>::new(userdata as *mut _) else {
                    return 0;
                };
                let EventSourceTime {
                    handler,
                    event_source,
                    ..
                } = ptr.as_mut();
                let next_usec =
                    (handler)(EventSourceHandle(Pin::new_unchecked(ptr.as_mut())), usec);
                match next_usec {
                    Usec::Absolute(x) => {
                        ffi::sd_event_source_set_time(*event_source, x);
                    }
                    #[cfg(feature = "systemd_v247")]
                    Usec::Relative(x) => {
                        ffi::sd_event_source_set_time_relative(*event_source, x);
                    }
                }
                0
            }

            let res = match usec {
                Usec::Absolute(x) => ffi::sd_event_add_time(
                    event,
                    &mut source.event_source(),
                    clock as _,
                    x,
                    accuracy,
                    Some(handler),
                    source.as_mut_ptr() as _,
                ),
                #[cfg(feature = "systemd_v247")]
                Usec::Absolute(x) => ffi::sd_event_add_time_relative(
                    event,
                    &mut source.event_source(),
                    clock as _,
                    x,
                    accuracy,
                    Some(handler),
                    source.as_mut_ptr() as _,
                ),
            };
            assert!(
                res >= 0,
                "Could not add time event handler to the event loop."
            );

            assert!(!source.event_source().is_null());

            source
        }
    }

    pub fn add_io<IO: AsRawFd + 'static>(
        mut self: Pin<&mut Self>,
        mut io: IO,
        events: Events,
        mut callback: impl FnMut(EventSourceHandle<&mut EventSourceIo>, &mut IO, Events) + 'static,
    ) -> EventSourceHandle<&mut EventSourceIo> {
        unsafe {
            let event = self.event;
            let fd = io.as_raw_fd();
            let mut source = {
                let event = self.as_mut().as_mut_ptr();
                self.add_event_source(EventSourceIo {
                    event_source: ptr::null_mut(),
                    event,
                    handler: Box::new(move |event_source, events| {
                        callback(event_source, &mut io, events)
                    }),
                    borrowed: false,
                    _pin: Default::default(),
                })
                .as_io()
                .unwrap()
            };

            unsafe extern "C" fn handler(
                _event_source: *mut ffi::sd_event_source,
                _fd: RawFd,
                revents: u32,
                userdata: *mut c_void,
            ) -> i32 {
                let Some(mut ptr) = ptr::NonNull::<EventSourceIo>::new(userdata as *mut _) else {
                    return 0;
                };
                (ptr.as_mut().handler)(
                    EventSourceHandle(Pin::new_unchecked(ptr.as_mut())),
                    Events(revents),
                );
                0
            }

            assert!(
                fd_nonblock(fd, true) >= 0,
                "Could not set the file descriptor to nonblocking."
            );

            assert!(
                ffi::sd_event_add_io(
                    event,
                    &mut source.event_source(),
                    fd,
                    events.0,
                    Some(handler),
                    source.as_mut_ptr() as _,
                ) >= 0,
                "Could not add io event handler to the event loop."
            );

            assert!(!source.event_source().is_null());

            source
        }
    }

    pub fn add_signal(
        mut self: Pin<&mut Self>,
        signal: Signal,
        callback: impl FnMut(EventSourceHandle<&mut EventSourceSignal>, &SignalInfo) + 'static,
    ) -> EventSourceHandle<&mut EventSourceSignal> {
        unsafe {
            let event = self.event;
            let mut source = {
                let event = self.as_mut().as_mut_ptr();
                self.add_event_source(EventSourceSignal {
                    event_source: ptr::null_mut(),
                    event,
                    handler: Some(Box::new(callback)),
                    borrowed: false,
                    _pin: Default::default(),
                })
                .as_signal()
                .unwrap()
            };

            unsafe extern "C" fn handler(
                _event_source: *mut ffi::sd_event_source,
                sig_info: *const libsystemd_sys::signalfd_siginfo,
                userdata: *mut c_void,
            ) -> i32 {
                let Some(mut ptr) = ptr::NonNull::<EventSourceSignal>::new(userdata as *mut _)
                else {
                    return 0;
                };
                if let Some(handler) = ptr.as_mut().handler.as_mut() {
                    (handler)(
                        EventSourceHandle(Pin::new_unchecked(ptr.as_mut())),
                        &*sig_info,
                    );
                }
                0
            }

            block_signal(signal);

            assert!(
                ffi::sd_event_add_signal(
                    event,
                    &mut source.event_source(),
                    signal.0,
                    Some(handler),
                    source.as_mut_ptr() as _,
                ) >= 0,
                "Could not add signal event handler to the event loop."
            );

            assert!(!source.event_source().is_null());

            source
        }
    }

    pub fn add_signal_default(
        mut self: Pin<&mut Self>,
        signal: Signal,
    ) -> EventSourceHandle<&mut EventSourceSignal> {
        unsafe {
            let event = self.event;
            let mut source = {
                let event = self.as_mut().as_mut_ptr();
                self.add_event_source(EventSourceSignal {
                    event_source: ptr::null_mut(),
                    event,
                    handler: None,
                    borrowed: false,
                    _pin: Default::default(),
                })
                .as_signal()
                .unwrap()
            };

            block_signal(signal);

            assert!(
                ffi::sd_event_add_signal(
                    event,
                    &mut source.event_source(),
                    signal.0,
                    None,
                    source.as_mut_ptr() as _,
                ) >= 0,
                "Could not add signal event handler to the event loop."
            );

            assert!(!source.event_source().is_null());

            source
        }
    }

    pub fn userdata<T: Default + 'static>(self: Pin<&mut Self>) -> &mut T {
        unsafe {
            self.get_unchecked_mut()
                .userdata
                .entry(TypeId::of::<T>())
                .or_insert_with(|| Box::new(T::default()))
                .downcast_mut()
                .unwrap()
        }
    }

    pub fn get_event_source_time(
        self: Pin<&mut Self>,
        id: EventSourceId,
    ) -> Option<EventSourceHandle<&mut EventSourceTime>> {
        unsafe {
            self.get_unchecked_mut()
                .event_sources
                .iter_mut()
                .flat_map(|x| x.as_time())
                .find(|x| x.id() == id)
        }
    }

    pub fn get_event_source_io(
        self: Pin<&mut Self>,
        id: EventSourceId,
    ) -> Option<EventSourceHandle<&mut EventSourceIo>> {
        unsafe {
            self.get_unchecked_mut()
                .event_sources
                .iter_mut()
                .flat_map(|x| x.as_io())
                .find(|x| x.id() == id)
        }
    }

    pub fn get_event_source_signal(
        self: Pin<&mut Self>,
        id: EventSourceId,
    ) -> Option<EventSourceHandle<&mut EventSourceSignal>> {
        unsafe {
            self.get_unchecked_mut()
                .event_sources
                .iter_mut()
                .flat_map(|x| x.as_signal())
                .find(|x| x.id() == id)
        }
    }
}

pub enum Usec {
    Absolute(u64),
    #[cfg(feature = "systemd_v247")]
    Relative(u64),
}

#[repr(i32)]
pub enum Clock {
    Realtime = libc::CLOCK_REALTIME,
    RealtimeAlarm = libc::CLOCK_REALTIME_ALARM,
    Monotonic = libc::CLOCK_MONOTONIC,
    Boottime = libc::CLOCK_BOOTTIME,
    BoottimeAlarm = libc::CLOCK_BOOTTIME_ALARM,
}

enum EventSource {
    Time(EventSourceTime),
    Io(EventSourceIo),
    Signal(EventSourceSignal),
}

impl Drop for EventSource {
    fn drop(&mut self) {
        unsafe {
            if !self.event_source().is_null() {
                assert!(
                    ffi::sd_event_source_unref(self.event_source()).is_null(),
                    "Cannot destroy event source."
                );
            }
        }
    }
}

impl EventSource {
    fn event_source(&self) -> *mut ffi::sd_event_source {
        match self {
            EventSource::Time(EventSourceTime { event_source, .. }) => *event_source,
            EventSource::Io(EventSourceIo { event_source, .. }) => *event_source,
            EventSource::Signal(EventSourceSignal { event_source, .. }) => *event_source,
        }
    }

    // TODO remove?
    unsafe fn id(&self) -> EventSourceId {
        EventSourceId::new_unchecked(self.event_source() as _)
    }

    unsafe fn as_time(&mut self) -> Option<EventSourceHandle<&mut EventSourceTime>> {
            match self {
                EventSource::Time(x) => x.try_borrow(),
                _ => None,
            }
    }

    unsafe fn as_io(&mut self) -> Option<EventSourceHandle<&mut EventSourceIo>> {
            match self {
                EventSource::Io(x) => x.try_borrow(),
                _ => None,
            }
    }

    unsafe fn as_signal(&mut self) -> Option<EventSourceHandle<&mut EventSourceSignal>> {
            match self {
                EventSource::Signal(x) => x.try_borrow(),
                _ => None,
            }
    }
}

pub struct EventSourceHandle<T>(Pin<T>);

impl<T> EventSourceHandle<&mut T> {
    #[inline]
    fn as_mut(&mut self) -> Pin<&mut T> {
        self.0.as_mut()
    }

    #[inline]
    fn as_ref(&self) -> Pin<&T> {
        self.0.as_ref()
    }

    #[inline]
    unsafe fn get_unchecked_mut(&mut self) -> &mut T {
        self.as_mut().get_unchecked_mut()
    }
}

impl<T> Drop for EventSourceHandle<T> {
    fn drop(&mut self) {
    }
}

pub type EventSourceId = num::NonZeroU64;

macro_rules! impl_event_source_base_methods {
    ($ty:ty) => {
        impl EventSourceHandle<&mut $ty> {
            #[inline]
            unsafe fn as_mut_ptr(&mut self) -> *mut $ty {
                self.get_unchecked_mut() as *mut _
            }

            #[inline]
            fn event_source(&mut self) -> *mut ffi::sd_event_source {
                self.as_mut().event_source
            }

            #[inline]
            pub fn enable(&mut self) {
                unsafe {
                    ffi::sd_event_source_set_enabled(self.as_mut().event_source, ffi::SD_EVENT_ON);
                }
            }

            #[inline]
            pub fn disable(&mut self) {
                unsafe {
                    ffi::sd_event_source_set_enabled(self.as_mut().event_source, ffi::SD_EVENT_OFF);
                }
            }

            #[inline]
            pub fn oneshot(&mut self) {
                unsafe {
                    ffi::sd_event_source_set_enabled(
                        self.as_mut().event_source,
                        ffi::SD_EVENT_ONESHOT,
                    );
                }
            }

            #[inline]
            pub fn id(&self) -> EventSourceId {
                unsafe { EventSourceId::new_unchecked(self.as_ref().event_source as _) }
            }

            #[inline]
            pub fn event(&self) -> Pin<&mut Event> {
                unsafe { Event::from_mut_ptr(self.as_ref().event) }
            }
        }

        impl $ty {
            #[inline]
            unsafe fn try_borrow(&mut self) -> Option<EventSourceHandle<&mut $ty>> {
                if self.borrowed {
                    None
                } else {
                    self.borrowed = true;
                    Some(EventSourceHandle(Pin::new_unchecked(self)))
                }
            }

            #[inline]
            fn free_borrow(&mut self) {
                self.borrowed = false;
            }
        }
    };
}

pub struct EventSourceTime {
    event_source: *mut ffi::sd_event_source,
    event: *mut Event,
    handler: Box<dyn FnMut(EventSourceHandle<&mut EventSourceTime>, u64) -> Usec>,
    borrowed: bool,
    _pin: std::marker::PhantomPinned,
}

impl From<EventSourceTime> for EventSource {
    fn from(event_source: EventSourceTime) -> EventSource {
        EventSource::Time(event_source)
    }
}

impl_event_source_base_methods!(EventSourceTime);

impl EventSourceHandle<&mut EventSourceTime> {
    pub fn set_time(&mut self, usec: Usec) {
        unsafe {
            match usec {
                Usec::Absolute(x) => {
                    ffi::sd_event_source_set_time(self.event_source(), x);
                }
                #[cfg(feature = "systemd_v247")]
                Usec::Relative(x) => {
                    ffi::sd_event_source_set_time_relative(self.event_source(), x);
                }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct Events(u32);

impl Events {
    pub const EPOLLIN: Self = Events(libc::EPOLLIN as _);
    pub const EPOLLOUT: Self = Events(libc::EPOLLOUT as _);
    pub const EPOLLRDHUP: Self = Events(libc::EPOLLRDHUP as _);
    pub const EPOLLPRI: Self = Events(libc::EPOLLPRI as _);
    pub const EPOLLET: Self = Events(libc::EPOLLET as _);
    pub const EPOLLERR: Self = Events(libc::EPOLLERR as _);
    pub const EPOLLHUP: Self = Events(libc::EPOLLHUP as _);
}

impl std::ops::BitOr for Events {
    type Output = Events;

    fn bitor(self, rhs: Events) -> Events {
        Events(self.0 | rhs.0)
    }
}

impl std::ops::BitAnd for Events {
    type Output = Events;

    fn bitand(self, rhs: Events) -> Events {
        Events(self.0 & rhs.0)
    }
}

impl std::ops::BitXor for Events {
    type Output = Events;

    fn bitxor(self, rhs: Events) -> Events {
        Events(self.0 ^ rhs.0)
    }
}

impl Iterator for Events {
    type Item = Events;

    fn next(&mut self) -> Option<Events> {
        for i in 0..32 {
            let mask = 1 << i;
            if self.0 & mask > 0 {
                self.0 ^= mask;
                return Some(Events(mask));
            }
        }
        None
    }
}

impl Events {
    #[inline]
    pub fn handle(&mut self, event: Events, mut callback: impl FnMut()) -> &mut Self {
        if (self.0 & event.0) > 0 {
            (callback)();
        }
        self
    }

    #[inline]
    pub fn end(&mut self) {
        ()
    }
}

pub struct EventSourceIo {
    event_source: *mut ffi::sd_event_source,
    event: *mut Event,
    handler: Box<dyn FnMut(EventSourceHandle<&mut EventSourceIo>, Events)>,
    borrowed: bool,
    _pin: std::marker::PhantomPinned,
}

impl From<EventSourceIo> for EventSource {
    fn from(event_source: EventSourceIo) -> EventSource {
        EventSource::Io(event_source)
    }
}

impl_event_source_base_methods!(EventSourceIo);

impl EventSourceHandle<&mut EventSourceIo> {
    pub fn get_events(&mut self) -> Events {
        unsafe {
            let mut events = Events(0);
            assert!(
                ffi::sd_event_source_get_io_events(self.event_source(), &mut events.0) >= 0,
                "Could not set events on IO event handler."
            );
            events
        }
    }

    pub fn set_events(&mut self, events: Events) {
        unsafe {
            assert!(
                ffi::sd_event_source_set_io_events(self.event_source(), events.0) >= 0,
                "Could not set events on IO event handler."
            );
        }
    }

    pub fn add_events(&mut self, new_events: Events) {
        let existing_events = self.get_events();
        self.set_events(existing_events | new_events);
    }

    pub fn remove_events(&mut self, events: Events) {
        let existing_events = self.get_events();
        self.set_events(existing_events ^ events);
    }

    pub fn raw_fd(&mut self) -> BorrowedFd {
        unsafe { BorrowedFd::borrow_raw(ffi::sd_event_source_get_io_fd(self.event_source())) }
    }

    pub fn replace<IO: AsRawFd + 'static>(
        &mut self,
        mut io: IO,
        mut callback: impl FnMut(EventSourceHandle<&mut EventSourceIo>, &mut IO, Events) + 'static,
    ) {
        unsafe {
            let id = self.id();
            let event = self.event();
            let fd = io.as_raw_fd();

            self.0.as_mut().get_unchecked_mut().handler =
                Box::new(move |event_source, events| callback(event_source, &mut io, events));

            assert!(
                ffi::sd_event_source_set_io_fd(self.event_source(), fd) >= 0,
                "Failed to set the event handler's file descriptor."
            );
        }
    }

    pub fn drop(self) {
        unsafe {
            self.event().remove_event_source(self.id());
        }
    }
}

pub struct EventSourceSignal {
    event_source: *mut ffi::sd_event_source,
    event: *mut Event,
    handler: Option<Box<dyn FnMut(EventSourceHandle<&mut EventSourceSignal>, &SignalInfo)>>,
    borrowed: bool,
    _pin: std::marker::PhantomPinned,
}

impl From<EventSourceSignal> for EventSource {
    fn from(event_source: EventSourceSignal) -> EventSource {
        EventSource::Signal(event_source)
    }
}

impl_event_source_base_methods!(EventSourceSignal);

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct Signal(i32);

impl Signal {
    pub const HUP: Signal = Signal(libc::SIGHUP);
    pub const INT: Signal = Signal(libc::SIGINT);
    pub const QUIT: Signal = Signal(libc::SIGQUIT);
    pub const ILL: Signal = Signal(libc::SIGILL);
    pub const TRAP: Signal = Signal(libc::SIGTRAP);
    pub const ABRT: Signal = Signal(libc::SIGABRT);
    pub const IOT: Signal = Signal(libc::SIGIOT);
    pub const BUS: Signal = Signal(libc::SIGBUS);
    pub const FPE: Signal = Signal(libc::SIGFPE);
    pub const KILL: Signal = Signal(libc::SIGKILL);
    pub const USR1: Signal = Signal(libc::SIGUSR1);
    pub const SEGV: Signal = Signal(libc::SIGSEGV);
    pub const USR2: Signal = Signal(libc::SIGUSR2);
    pub const PIPE: Signal = Signal(libc::SIGPIPE);
    pub const ALRM: Signal = Signal(libc::SIGALRM);
    pub const TERM: Signal = Signal(libc::SIGTERM);
    pub const STKFLT: Signal = Signal(libc::SIGSTKFLT);
    pub const CHLD: Signal = Signal(libc::SIGCHLD);
    pub const CONT: Signal = Signal(libc::SIGCONT);
    pub const STOP: Signal = Signal(libc::SIGSTOP);
    pub const TSTP: Signal = Signal(libc::SIGTSTP);
    pub const TTIN: Signal = Signal(libc::SIGTTIN);
    pub const TTOU: Signal = Signal(libc::SIGTTOU);
    pub const URG: Signal = Signal(libc::SIGURG);
    pub const XCPU: Signal = Signal(libc::SIGXCPU);
    pub const XFSZ: Signal = Signal(libc::SIGXFSZ);
    pub const VTALRM: Signal = Signal(libc::SIGVTALRM);
    pub const PROF: Signal = Signal(libc::SIGPROF);
    pub const WINCH: Signal = Signal(libc::SIGWINCH);
    pub const IO: Signal = Signal(libc::SIGIO);
    pub const POLL: Signal = Signal(libc::SIGPOLL);
    pub const PWR: Signal = Signal(libc::SIGPWR);
    pub const SYS: Signal = Signal(libc::SIGSYS);
}

pub type SignalInfo = libsystemd_sys::signalfd_siginfo;

unsafe fn fd_nonblock(fd: RawFd, nonblock: bool) -> i32 {
    assert!(fd >= 0);

    let flags = libc::fcntl(fd, libc::F_GETFL, 0);
    assert!(flags >= 0, "Could not retrieve flag from file descriptor.");

    let nflags = if nonblock {
        flags | libc::O_NONBLOCK
    } else {
        flags ^ libc::O_NONBLOCK
    };

    if nflags == flags {
        return 0;
    }

    libc::fcntl(fd, libc::F_SETFL, nflags)
}

unsafe fn block_signal(signal: Signal) {
    let mut set: libc::sigset_t = mem::zeroed();
    libc::sigemptyset(&mut set);
    libc::sigaddset(&mut set, signal.0);
    assert!(
        libc::sigprocmask(libc::SIG_BLOCK, &set, ptr::null_mut()) >= 0,
        "Could not block signal."
    );
}
