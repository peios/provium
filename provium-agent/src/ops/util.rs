//! Cross-cutting helpers shared by op handlers.

use std::io;

use provium_protocol::OsError;

/// Map a [`std::io::Error`] to the wire-shaped [`OsError`].
///
/// The `io::Error` may have been built from a raw OS errno (typical) or
/// fabricated by std (`ErrorKind::InvalidInput`, etc.). For the latter,
/// errno is `0` — the host's error rendering knows to suppress the
/// "errno N" suffix when N is 0.
pub(crate) fn os_error_from_io(e: io::Error) -> OsError {
    OsError {
        errno: e.raw_os_error().unwrap_or(0),
        message: e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_real_errno() {
        let e = io::Error::from_raw_os_error(2);
        let os = os_error_from_io(e);
        assert_eq!(os.errno, 2);
        assert!(!os.message.is_empty());
    }

    #[test]
    fn maps_synthesised_error() {
        let e = io::Error::new(io::ErrorKind::InvalidInput, "bad");
        let os = os_error_from_io(e);
        assert_eq!(os.errno, 0);
        assert_eq!(os.message, "bad");
    }
}
