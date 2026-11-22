//! Built-in defaults of the tuning switches. An experiment build changes one
//! line here; the environment variables named beside each still override it.

/// Bind each worker thread to one CPU (PIPE_PIN=1 / PIPE_NO_PIN=1). When on
/// by default it applies only if the process may use exactly as many CPUs as
/// it has threads.
pub const PIN: bool = true;

/// Kilobytes of text scanned and parsed at a time (PIPE_BLOCK_KB).
pub const BLOCK_KB: usize = 32;

/// Ask for transparent huge pages (PIPE_NO_HUGE=1 turns them off, PIPE_HUGE=1 on).
pub const HUGE: bool = true;

/// Worker threads = usable CPUs divided by this (PIPE_THREADS sets the number).
pub const THREAD_DIVISOR: usize = 1;
