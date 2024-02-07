use libsystemd_sys::event as ffi;
use std::any::*;
use std::cell::*;
use std::collections::HashMap;
use std::ffi::*;
use std::mem;
use std::num;
use std::os::fd::*;
use std::pin::*;
use std::ptr;

pub struct Event {
    event: *mut ffi::sd_event,
    event_sources: Vec<RefCell<EventSource>>,
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
        callback: impl FnOnce(&'a mut Vec<RefCell<EventSource>>) -> T,
    ) -> T {
        callback(&mut self.get_unchecked_mut().event_sources)
    }

    unsafe fn add_event_source(
        self: Pin<&mut Self>,
        event_source: impl Into<EventSource>,
    ) -> &mut EventSource {
        self.with_event_sources(|vec| {
            vec.push(RefCell::new(event_source.into()));
            vec.last_mut().unwrap().get_mut()
        })
    }

    unsafe fn unregister_event_source(self: Pin<&mut Self>, id: EventSourceId) {
        self.with_event_sources(|vec| {
            if let Some(i) = vec.iter_mut().position(|x| x.get_mut().id() == id) {
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
        callback: impl FnMut(EventSourceHandle<EventSourceTime>, u64) -> Usec + 'static,
    ) -> EventSourceHandle<EventSourceTime> {
        unsafe {
            let event = self.event;
            let source = {
                let event = self.as_mut().as_mut_ptr();
                self.as_mut().add_event_source(EventSourceTime {
                    event_source: ptr::null_mut(),
                    event,
                    handler: Box::new(callback),
                    _pin: Default::default(),
                })
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
                    ref mut handler,
                    event_source,
                    event,
                    ..
                } = *ptr.as_mut();
                let event = Event::from_mut_ptr(event);
                if let Some(handle) = event.get_event_source_time(ptr.as_mut().id()) {
                    let next_usec = (handler)(handle, usec);
                    match next_usec {
                        Usec::Absolute(x) => {
                            ffi::sd_event_source_set_time(event_source, x);
                        }
                        #[cfg(feature = "systemd_v247")]
                        Usec::Relative(x) => {
                            ffi::sd_event_source_set_time_relative(event_source, x);
                        }
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

            let id = source.id();
            self.get_event_source_time(id).unwrap()
        }
    }

    pub fn add_io<IO: AsRawFd + 'static>(
        mut self: Pin<&mut Self>,
        mut io: IO,
        events: Events,
        mut callback: impl FnMut(EventSourceHandle<EventSourceIo>, &mut IO, Events) + 'static,
    ) -> EventSourceHandle<EventSourceIo> {
        unsafe {
            let event = self.event;
            let fd = io.as_raw_fd();
            let source = {
                let event = self.as_mut().as_mut_ptr();
                self.as_mut()
                    .add_event_source(EventSourceIo {
                        event_source: ptr::null_mut(),
                        event,
                        handler: Box::new(move |event_source, events| {
                            callback(event_source, &mut io, events)
                        }),
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
                let EventSourceIo {
                    event,
                    ref mut handler,
                    ..
                } = *ptr.as_mut();
                let event = Event::from_mut_ptr(event);
                if let Some(handle) = event.get_event_source_io(ptr.as_mut().id()) {
                    (handler)(handle, Events(revents));
                }
                0
            }

            assert!(
                fd_nonblock(fd, true) >= 0,
                "Could not set the file descriptor to nonblocking."
            );

            assert!(
                ffi::sd_event_add_io(
                    event,
                    &mut source.event_source,
                    fd,
                    events.0,
                    Some(handler),
                    source.as_mut_ptr() as _,
                ) >= 0,
                "Could not add io event handler to the event loop."
            );

            assert!(!source.event_source.is_null());

            let id = source.id();
            self.get_event_source_io(id).unwrap()
        }
    }

    pub fn add_signal(
        mut self: Pin<&mut Self>,
        signal: Signal,
        callback: impl FnMut(EventSourceHandle<EventSourceSignal>, &SignalInfo) + 'static,
    ) -> EventSourceHandle<EventSourceSignal> {
        unsafe {
            let event = self.event;
            let source = {
                let event = self.as_mut().as_mut_ptr();
                self.as_mut()
                    .add_event_source(EventSourceSignal {
                        event_source: ptr::null_mut(),
                        event,
                        handler: Some(Box::new(callback)),
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
                let EventSourceSignal {
                    ref mut handler,
                    event,
                    ..
                } = *ptr.as_mut();
                let event = Event::from_mut_ptr(event);
                if let Some(handler) = handler {
                    if let Some(handle) = event.get_event_source_signal(ptr.as_mut().id()) {
                        (handler)(handle, &*sig_info);
                    }
                }
                0
            }

            block_signal(signal);

            assert!(
                ffi::sd_event_add_signal(
                    event,
                    &mut source.event_source,
                    signal.0,
                    Some(handler),
                    source.as_mut_ptr() as _,
                ) >= 0,
                "Could not add signal event handler to the event loop."
            );

            assert!(!source.event_source.is_null());

            let id = source.id();
            self.get_event_source_signal(id).unwrap()
        }
    }

    pub fn add_signal_default(
        mut self: Pin<&mut Self>,
        signal: Signal,
    ) -> EventSourceHandle<EventSourceSignal> {
        unsafe {
            let event = self.event;
            let source = {
                let event = self.as_mut().as_mut_ptr();
                self.as_mut()
                    .add_event_source(EventSourceSignal {
                        event_source: ptr::null_mut(),
                        event,
                        handler: None,
                        _pin: Default::default(),
                    })
                    .as_signal()
                    .unwrap()
            };

            block_signal(signal);

            assert!(
                ffi::sd_event_add_signal(
                    event,
                    &mut source.event_source,
                    signal.0,
                    None,
                    source.as_mut_ptr() as _,
                ) >= 0,
                "Could not add signal event handler to the event loop."
            );

            assert!(!source.event_source.is_null());

            let id = source.id();
            self.get_event_source_signal(id).unwrap()
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
    ) -> Option<EventSourceHandle<EventSourceTime>> {
        unsafe {
            self.get_unchecked_mut()
                .event_sources
                .iter_mut()
                .flat_map(|x| x.try_borrow_mut())
                .find(|x| x.id() == id && x.is_time())
                .map(|x| RefMut::map(x, |x| x.as_time().unwrap()))
                .map(|x| EventSourceHandle(Pin::new_unchecked(x)))
        }
    }

    pub fn get_event_source_io(
        self: Pin<&mut Self>,
        id: EventSourceId,
    ) -> Option<EventSourceHandle<EventSourceIo>> {
        unsafe {
            self.get_unchecked_mut()
                .event_sources
                .iter_mut()
                .flat_map(|x| x.try_borrow_mut())
                .find(|x| x.id() == id && x.is_io())
                .map(|x| RefMut::map(x, |x| x.as_io().unwrap()))
                .map(|x| EventSourceHandle(Pin::new_unchecked(x)))
        }
    }

    pub fn get_event_source_signal(
        self: Pin<&mut Self>,
        id: EventSourceId,
    ) -> Option<EventSourceHandle<EventSourceSignal>> {
        unsafe {
            self.get_unchecked_mut()
                .event_sources
                .iter_mut()
                .flat_map(|x| x.try_borrow_mut())
                .find(|x| x.id() == id && x.is_signal())
                .map(|x| RefMut::map(x, |x| x.as_signal().unwrap()))
                .map(|x| EventSourceHandle(Pin::new_unchecked(x)))
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

    unsafe fn id(&self) -> EventSourceId {
        EventSourceId::new_unchecked(self.event_source() as _)
    }

    unsafe fn as_mut_ptr(&mut self) -> *mut libc::c_void {
        match self {
            EventSource::Time(x) => x.as_mut_ptr() as _,
            EventSource::Io(x) => x.as_mut_ptr() as _,
            EventSource::Signal(x) => x.as_mut_ptr() as _,
        }
    }

    unsafe fn is_time(&self) -> bool {
        match self {
            EventSource::Time(_) => true,
            _ => false,
        }
    }

    unsafe fn as_time(&mut self) -> Option<&mut EventSourceTime> {
        match self {
            EventSource::Time(x) => Some(x),
            _ => None,
        }
    }

    unsafe fn is_io(&self) -> bool {
        match self {
            EventSource::Io(_) => true,
            _ => false,
        }
    }

    unsafe fn as_io(&mut self) -> Option<&mut EventSourceIo> {
        match self {
            EventSource::Io(x) => Some(x),
            _ => None,
        }
    }

    unsafe fn is_signal(&self) -> bool {
        match self {
            EventSource::Signal(_) => true,
            _ => false,
        }
    }

    unsafe fn as_signal(&mut self) -> Option<&mut EventSourceSignal> {
        match self {
            EventSource::Signal(x) => Some(x),
            _ => None,
        }
    }
}

pub struct EventSourceHandle<'a, T>(Pin<RefMut<'a, T>>);

impl<T> EventSourceHandle<'_, T> {
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

pub type EventSourceId = num::NonZeroU64;

macro_rules! impl_event_source_base_methods {
    ($ty:ty) => {
        impl EventSourceHandle<'_, $ty> {
            #[inline]
            pub fn enable(&mut self) {
                unsafe {
                    ffi::sd_event_source_set_enabled(self.as_ref().event_source, ffi::SD_EVENT_ON);
                }
            }

            #[inline]
            pub fn disable(&mut self) {
                unsafe {
                    ffi::sd_event_source_set_enabled(self.as_ref().event_source, ffi::SD_EVENT_OFF);
                }
            }

            #[inline]
            pub fn oneshot(&mut self) {
                unsafe {
                    ffi::sd_event_source_set_enabled(
                        self.as_ref().event_source,
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
            fn id(&self) -> EventSourceId {
                unsafe { EventSourceId::new_unchecked(self.event_source as _) }
            }

            #[inline]
            unsafe fn as_mut_ptr(&mut self) -> *mut $ty {
                self as *mut _
            }
        }
    };
}

pub struct EventSourceTime {
    event_source: *mut ffi::sd_event_source,
    event: *mut Event,
    handler: Box<dyn FnMut(EventSourceHandle<EventSourceTime>, u64) -> Usec>,
    _pin: std::marker::PhantomPinned,
}

impl From<EventSourceTime> for EventSource {
    fn from(event_source: EventSourceTime) -> EventSource {
        EventSource::Time(event_source)
    }
}

impl_event_source_base_methods!(EventSourceTime);

impl EventSourceHandle<'_, EventSourceTime> {
    pub fn set_time(&mut self, usec: Usec) {
        unsafe {
            match usec {
                Usec::Absolute(x) => {
                    ffi::sd_event_source_set_time(self.as_ref().event_source, x);
                }
                #[cfg(feature = "systemd_v247")]
                Usec::Relative(x) => {
                    ffi::sd_event_source_set_time_relative(self.as_ref().event_source, x);
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
    handler: Box<dyn FnMut(EventSourceHandle<EventSourceIo>, Events)>,
    _pin: std::marker::PhantomPinned,
}

impl From<EventSourceIo> for EventSource {
    fn from(event_source: EventSourceIo) -> EventSource {
        EventSource::Io(event_source)
    }
}

impl_event_source_base_methods!(EventSourceIo);

impl EventSourceHandle<'_, EventSourceIo> {
    pub fn get_events(&mut self) -> Events {
        unsafe {
            let mut events = Events(0);
            assert!(
                ffi::sd_event_source_get_io_events(self.as_ref().event_source, &mut events.0) >= 0,
                "Could not set events on IO event handler."
            );
            events
        }
    }

    pub fn set_events(&mut self, events: Events) {
        unsafe {
            assert!(
                ffi::sd_event_source_set_io_events(self.as_ref().event_source, events.0) >= 0,
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
        unsafe {
            BorrowedFd::borrow_raw(ffi::sd_event_source_get_io_fd(self.as_ref().event_source))
        }
    }

    pub fn replace<IO: AsRawFd + 'static>(
        &mut self,
        mut io: IO,
        mut callback: impl FnMut(EventSourceHandle<EventSourceIo>, &mut IO, Events) + 'static,
    ) {
        unsafe {
            assert!(
                ffi::sd_event_source_set_io_fd(self.as_ref().event_source, io.as_raw_fd()) >= 0,
                "Failed to set the event handler's file descriptor."
            );

            self.get_unchecked_mut().handler =
                Box::new(move |event_source, events| callback(event_source, &mut io, events));
        }
    }

    pub fn unregister(self) {
        unsafe {
            self.event().unregister_event_source(self.id());
        }
    }
}

pub struct EventSourceSignal {
    event_source: *mut ffi::sd_event_source,
    event: *mut Event,
    handler: Option<Box<dyn FnMut(EventSourceHandle<EventSourceSignal>, &SignalInfo)>>,
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
