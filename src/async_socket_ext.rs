// SPDX-License-Identifier: MIT

use std::{
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll},
};

use bytes::{buf::UninitSlice, BufMut};

use crate::{AsyncSocket, SocketAddr};

/// Support trait for [`AsyncSocket`]
///
/// Provides awaitable variants of the poll functions from [`AsyncSocket`].
pub trait AsyncSocketExt: AsyncSocket {
    /// `async fn send(&mut self, buf: &[u8]) -> io::Result<usize>`
    fn send<'a, 'b>(&'a mut self, buf: &'b [u8]) -> PollSend<'a, 'b, Self> {
        PollSend { socket: self, buf }
    }

    /// `async fn send(&mut self, buf: &[u8]) -> io::Result<usize>`
    fn send_to<'a, 'b>(
        &'a mut self,
        buf: &'b [u8],
        addr: &'b SocketAddr,
    ) -> PollSendTo<'a, 'b, Self> {
        PollSendTo {
            socket: self,
            buf,
            addr,
        }
    }

    /// `async fn recv<B>(&mut self, buf: &mut [u8]) -> io::Result<usize>`
    fn recv<'a, 'b>(&'a mut self, buf: &'b mut [u8]) -> PollRecv<'a, 'b, Self> {
        PollRecv { socket: self, buf }
    }

    fn recv_from<'a, 'b>(
        &'a mut self,
        buf: &'b mut [u8],
    ) -> PollRecvFrom<'a, 'b, Self> {
        PollRecvFrom { socket: self, buf }
    }

    fn recv_from_full(&mut self) -> PollRecvFromFull<'_, Self> {
        PollRecvFromFull { socket: self }
    }

    fn poll_recv_buf<B: BufMut>(
        &mut self,
        cx: &mut Context<'_>,
        buf: &mut B,
    ) -> Poll<io::Result<()>> {
        let c = buf.chunk_mut();
        let len = c.len();
        let p = unsafe {
            &mut *(c as *mut _ as *mut [u8])
        };
        match self.poll_recv(cx, p) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(k) => Poll::Ready(k.map(|read| {
                let min = std::cmp::min(len, read);
                unsafe {
                    buf.advance_mut(min);
                }
            })),
        }
    }


    fn poll_recv_from_buf<B: BufMut>(
        &mut self,
        cx: &mut Context<'_>,
        buf: &mut B,
    ) -> Poll<io::Result<SocketAddr>> {
        let c = buf.chunk_mut();
        let len = c.len();
        let p = unsafe {
            &mut *(c as *mut _ as *mut [u8])
        };
        match self.poll_recv_from(cx, p) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(k) => Poll::Ready(k.map(|(read, addr)| {
                let min = std::cmp::min(len, read);
                unsafe {
                    buf.advance_mut(min);
                }
                addr
            })),
        }
    }

}

impl<S: AsyncSocket> AsyncSocketExt for S {}

pub struct PollSend<'a, 'b, S> {
    socket: &'a mut S,
    buf: &'b [u8],
}

impl<S> Future for PollSend<'_, '_, S>
where
    S: AsyncSocket,
{
    type Output = io::Result<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this: &mut Self = Pin::into_inner(self);
        this.socket.poll_send(cx, this.buf)
    }
}

pub struct PollSendTo<'a, 'b, S> {
    socket: &'a mut S,
    buf: &'b [u8],
    addr: &'b SocketAddr,
}

impl<S: AsyncSocket> Future for PollSendTo<'_, '_, S> {
    type Output = io::Result<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this: &mut Self = Pin::into_inner(self);
        this.socket.poll_send_to(cx, this.buf, this.addr)
    }
}

pub struct PollRecv<'a, 'b, S> {
    socket: &'a mut S,
    buf: &'b mut [u8],
}

impl<S> Future for PollRecv<'_, '_, S>
where
    S: AsyncSocket,
{
    type Output = io::Result<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this: &mut Self = Pin::into_inner(self);
        this.socket.poll_recv(cx, this.buf)
    }
}

pub struct PollRecvFrom<'a, 'b, S> {
    socket: &'a mut S,
    buf: &'b mut [u8],
}

impl<S> Future for PollRecvFrom<'_, '_, S>
where
    S: AsyncSocket,
{
    type Output = io::Result<(usize, SocketAddr)>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this: &mut Self = Pin::into_inner(self);
        this.socket.poll_recv_from(cx, this.buf)
    }
}

pub struct PollRecvFromFull<'a, S> {
    socket: &'a mut S,
}

impl<S> Future for PollRecvFromFull<'_, S>
where
    S: AsyncSocket,
{
    type Output = io::Result<(Vec<u8>, SocketAddr)>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this: &mut Self = Pin::into_inner(self);
        this.socket.poll_recv_from_full(cx)
    }
}
