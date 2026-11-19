use std::io;

// Winsock's address-family error from Microsoft's Windows Sockets Error Codes:
// https://learn.microsoft.com/en-us/windows/win32/winsock/windows-sockets-error-codes-2
#[cfg(windows)]
const WSAEAFNOSUPPORT: i32 = 10_047;

pub(super) fn ipv6_unavailable(error: &io::Error) -> bool {
    match error.raw_os_error() {
        #[cfg(unix)]
        Some(code) if code == rustix::io::Errno::AFNOSUPPORT.raw_os_error() => true,
        #[cfg(windows)]
        Some(WSAEAFNOSUPPORT) => true,
        _ => matches!(
            error.kind(),
            io::ErrorKind::AddrNotAvailable | io::ErrorKind::Unsupported
        ),
    }
}

#[test]
fn unavailable_ipv6_error_kinds_are_recognized() {
    for kind in [io::ErrorKind::AddrNotAvailable, io::ErrorKind::Unsupported] {
        assert!(ipv6_unavailable(&io::Error::from(kind)), "{kind:?}");
    }
}

#[cfg(any(unix, windows))]
#[test]
fn unsupported_address_family_raw_error_is_recognized() {
    #[cfg(unix)]
    let code = rustix::io::Errno::AFNOSUPPORT.raw_os_error();
    #[cfg(windows)]
    let code = WSAEAFNOSUPPORT;

    assert!(ipv6_unavailable(&io::Error::from_raw_os_error(code)));
}

#[test]
fn unrelated_bind_errors_are_not_skipped() {
    for kind in [
        io::ErrorKind::AddrInUse,
        io::ErrorKind::PermissionDenied,
        io::ErrorKind::InvalidInput,
        io::ErrorKind::Other,
    ] {
        assert!(!ipv6_unavailable(&io::Error::from(kind)), "{kind:?}");
    }
    // Unrecognized OS errors must not be skipped just because they share the
    // Uncategorized kind used for EAFNOSUPPORT on some platforms.
    assert!(!ipv6_unavailable(&io::Error::from_raw_os_error(-1)));
}
