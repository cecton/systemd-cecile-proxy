use libsystemd_sys::const_iovec;
use libsystemd_sys::journal as ffi;

fn log(priority: &'static str, message: impl AsRef<str>) {
    unsafe {
        let array = [
            const_iovec::from_str(priority),
            const_iovec::from_str(message.as_ref()),
        ];
        assert!(ffi::sd_journal_sendv(array.as_ptr(), array.len() as _) >= 0);
    }
}

macro_rules! generate_log_function {
    ($($name:ident => $priority:literal,)*) => {
        $(
            pub fn $name(message: impl std::fmt::Display) {
                log(concat!("PRIORITY=", $priority), format!("MESSAGE={message}"))
            }
        )*
    }
}

generate_log_function!(
    log_emerg => 0,
    log_alert => 1,
    log_crit => 2,
    log_err => 3,
    log_warning => 4,
    log_notice => 5,
    log_info => 6,
    log_debug => 7,
);

#[macro_export]
macro_rules! log_emerg {
    ($($tt:tt)*) => {
        $crate::log_emerg(format!($($tt)*));
    }
}

#[macro_export]
macro_rules! log_alert {
    ($($tt:tt)*) => {
        $crate::log_alert(format!($($tt)*));
    }
}

#[macro_export]
macro_rules! log_crit {
    ($($tt:tt)*) => {
        $crate::log_crit(format!($($tt)*));
    }
}

#[macro_export]
macro_rules! log_err {
    ($($tt:tt)*) => {
        $crate::log_err(format!($($tt)*));
    }
}

#[macro_export]
macro_rules! log_warning {
    ($($tt:tt)*) => {
        $crate::log_warning(format!($($tt)*));
    }
}

#[macro_export]
macro_rules! log_notice {
    ($($tt:tt)*) => {
        $crate::log_notice(format!($($tt)*));
    }
}

#[macro_export]
macro_rules! log_info {
    ($($tt:tt)*) => {
        $crate::log_info(format!($($tt)*));
    }
}

#[macro_export]
macro_rules! log_debug {
    ($($tt:tt)*) => {
        $crate::log_debug(format!($($tt)*));
    }
}
