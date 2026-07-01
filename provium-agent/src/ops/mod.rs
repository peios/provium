//! Op handlers — one submodule per family. Each is the agent-side
//! counterpart of the matching submodule under
//! [`provium_protocol::wire::ops`].

pub mod clock;
pub mod exec;
pub mod file;
pub mod ioctl;
pub mod process;
pub mod read_mem;
pub mod stream;
pub mod syscall;
pub mod worker;

mod util;

pub(crate) use util::os_error_from_io;
