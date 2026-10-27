use std::io::{self, Write};

/// Locks CLI stdout, ignoring a consumer that closes its pipe early.
///
/// Only stdout writes and flushes suppress `BrokenPipe`; command validation and
/// cleanup continue normally. Long-lived transports use their own stdout.
#[must_use]
pub fn stdout() -> impl Write {
    IgnoreBrokenPipe(io::stdout().lock())
}

struct IgnoreBrokenPipe<W>(W);

impl<W: Write> Write for IgnoreBrokenPipe<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self.0.write(buffer) {
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(buffer.len()),
            result => result,
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.0.flush() {
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
            result => result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Consumer {
        remaining: usize,
        error: io::ErrorKind,
    }

    impl Write for Consumer {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Err(self.error.into());
            }
            let written = buffer.len().min(self.remaining);
            self.remaining -= written;
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(self.error.into())
        }
    }

    #[test]
    fn partial_consumption_and_broken_pipe_flush_are_ignored() {
        let mut writer = IgnoreBrokenPipe(Consumer {
            remaining: 3,
            error: io::ErrorKind::BrokenPipe,
        });
        writer
            .write_all(b"first chunk")
            .expect("partial consumption");
        writer.write_all(b"later chunk").expect("closed consumer");
        writer.flush().expect("closed consumer flush");
    }

    #[test]
    fn other_io_errors_are_propagated() {
        for kind in [io::ErrorKind::PermissionDenied, io::ErrorKind::WriteZero] {
            let mut writer = IgnoreBrokenPipe(Consumer {
                remaining: 0,
                error: kind,
            });
            assert_eq!(
                writer.write_all(b"chunk").expect_err("write error").kind(),
                kind
            );
            assert_eq!(writer.flush().expect_err("flush error").kind(), kind);
        }
    }
}
