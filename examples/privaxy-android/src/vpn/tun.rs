//! Async reader/writer over the tun descriptor `VpnService` hands down.
//!
//! Java detaches the descriptor, so this owns it: dropping the device closes it, and closing it is
//! what tears the VPN interface down. Reads and writes are whole IP packets — Android's tun
//! carries no packet-information header, unlike a Linux `TUN` opened without `IFF_NO_PI`.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub struct TunDevice {
    fd: AsyncFd<OwnedFd>,
}

impl TunDevice {
    pub fn new(fd: OwnedFd) -> io::Result<Self> {
        set_nonblocking(&fd)?;
        Ok(Self {
            fd: AsyncFd::new(fd)?,
        })
    }
}

fn set_nonblocking(fd: &OwnedFd) -> io::Result<()> {
    let raw = fd.as_raw_fd();
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

impl AsyncRead for TunDevice {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let mut guard = ready!(self.fd.poll_read_ready(cx))?;
            let unfilled = buf.initialize_unfilled();
            let read = guard.try_io(|fd| {
                let count =
                    unsafe { libc::read(fd.as_raw_fd(), unfilled.as_mut_ptr().cast(), unfilled.len()) };
                if count < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(count as usize)
                }
            });
            match read {
                Ok(Ok(count)) => {
                    buf.advance(count);
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(error)) => return Poll::Ready(Err(error)),
                // Readiness was stale; ask again.
                Err(_would_block) => continue,
            }
        }
    }
}

impl AsyncWrite for TunDevice {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            let mut guard = ready!(self.fd.poll_write_ready(cx))?;
            let written = guard.try_io(|fd| {
                let count = unsafe { libc::write(fd.as_raw_fd(), buf.as_ptr().cast(), buf.len()) };
                if count < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(count as usize)
                }
            });
            match written {
                Ok(result) => return Poll::Ready(result),
                Err(_would_block) => continue,
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
