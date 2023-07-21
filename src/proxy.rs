use std::marker::ConstParamTy;
use std::{
    collections::HashMap,
    convert::TryInto,
    error::Error,
    ffi::CString,
    fs::File,
    io,
    marker::PhantomData,
    mem::{forget, size_of},
    ops::Index,
    os::fd::{AsFd, AsRawFd, FromRawFd},
    path::PathBuf,
};
// use async_std::channel::unbounded;
use anyhow::Result;
use bytes::{buf::Limit, BufMut, Bytes, BytesMut};
use futures::future::Ready;
use futures::{ready, Future, SinkExt};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, ReadBuf},
    net::{UnixDatagram, UnixListener, UnixStream},
    sync::{mpsc::unbounded_channel, RwLock},
    task::spawn_blocking,
};

use crate::{AsyncSocket, AsyncSocketExt, SmolSocket, SocketAddr, TokioSocket};

/// Each proxySocket takes exclusive control of a UnixStream.
pub struct ProxySocket<'a, const TYP: ProxySocketType> {
    stream: UnixStream,
    header: Option<Window>,
    hbuf: Limit<BytesMut>,
    bytes_read: usize,
    proto: isize,
    _ctx: &'a PhantomData<()>,
    remaining_buflen: usize,
    last_addr: SocketAddr,
}

use std::task::Poll;

#[derive(ConstParamTy, PartialEq, Eq)]
pub enum ProxySocketType {
    PollRecvFrom,
    PollRecv,
    PollRecvFromFull,
}

impl<'a> AsyncSocket for ProxySocket<'a, { ProxySocketType::PollRecvFrom }> {
    type T = &'a mut ProxyCtxP<'a>;
    fn new(protocol: isize, ctx: Self::T) -> std::io::Result<Self> {
        let st = {
            // I have to block. I could refactor the trait tho.
            ctx.shared.pending.remove(&ctx.inode).ok_or(io::Error::new(
                io::ErrorKind::Other,
                "socket proxy daemon not present",
            ))?
        };
        let base = [0; Window::BUF_LEN];
        let mut buf = BytesMut::from(&base[..]);
        unsafe { buf.set_len(0) }

        Ok(Self {
            stream: st,
            header: None,
            hbuf: buf.limit(Window::BUF_LEN),
            bytes_read: 0,
            proto: protocol,
            _ctx: &PhantomData,
            remaining_buflen: 0,
            last_addr: SocketAddr::default()
        })
    }
    fn poll_send(
        &mut self,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut b = BytesMut::new();
        b.put_u32(buf.len().try_into().unwrap());
        b.put(&[0; SocketAddr::LEN][..]);
        b.put(buf);
        loop {
            ready!(self.stream.poll_write_ready(cx))?;

            match self.stream.try_write(&b) {
                Ok(s) => {
                    return Poll::Ready(Ok(s));
                }
                Err(e) => {
                    if matches!(e.kind(), io::ErrorKind::WouldBlock) {
                        continue;
                    } else {
                        return Poll::Ready(Err(e));
                    }
                }
            }
        }
    }
    /// This may need to be called repeatedly, as the buf may be not fully sent.
    /// Rtnetlink only tries it once tho.
    // XXX: must ensure it writes an entire netlink datagram in one go. And
    // normally it should.
    fn poll_send_to(
        &mut self,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
        addr: &crate::SocketAddr,
    ) -> std::task::Poll<io::Result<usize>> {
        let mut overhead = 0;
        let mut b = BytesMut::new();
        // TODO: It's possible that b is half written and the other side is
        // still expecting the buffer to continue.
        if self.remaining_buflen > 0 {
            if self.last_addr == *addr {
                // good
            } else {
                // we could fill up the bytes
                // but no. it's too bad
                unreachable!()
            }
        } else {
            b.put_u32(buf.len().try_into().unwrap());
            b.put(&addr.serialize()[..]);
            self.remaining_buflen = buf.len();
            overhead = b.len();
        }
        b.put(buf);
        loop {
            ready!(self.stream.poll_write_ready(cx))?;

            match self.stream.try_write(&b) {
                Ok(s) => {
                    // imitate the result
                    let adjusted = if s > overhead {
                        s - overhead
                    } else {
                        // buffer too small
                        unreachable!()
                    };
                    if self.remaining_buflen > 0 {
                        self.remaining_buflen -= adjusted;
                    }
                    return Poll::Ready(Ok(adjusted));
                }
                Err(e) => {
                    if matches!(e.kind(), io::ErrorKind::WouldBlock) {
                        continue;
                    } else {
                        return Poll::Ready(Err(e));
                    }
                }
            }
        }
    }
    fn poll_recv_from<B>(
        &mut self,
        cx: &mut std::task::Context<'_>,
        buf: &mut B,
    ) -> Poll<io::Result<crate::SocketAddr>>
    where
        B: bytes::BufMut,
    {
        // truncates the message to fit buffer
        // The theoritical max netlink packet size is 32KB fora a netlink
        // so rtnetlink uses this fn
        match self.poll_proxy(cx, buf) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(r) => match r {
                Ok(x) => match x {
                    PollRes::WithAddr(a) => Poll::Ready(Ok(a)),
                    _ => return Poll::Pending,
                },
                Err(e) => {
                    return Poll::Ready(Err(e));
                }
            },
        }
    }
}



/// remember to init
impl<'a, const TYP: ProxySocketType> ProxySocket<'a, TYP> {
    /// All upper layer polls should call this
    fn poll_proxy<B>(
        &mut self,
        cx: &mut std::task::Context<'_>,
        buf: &mut B,
    ) -> Poll<io::Result<PollRes>>
    where
        B: bytes::BufMut,
    {
        loop {
            ready!(self.stream.poll_read_ready(cx))?;
            let read_more_into_header = self.header.is_none();

            let r = if read_more_into_header {
                self.stream.try_read_buf(&mut self.hbuf)
            } else {
                // Bytes unread + Bytes read = Total Bytes
                // A byte is either read or unread.
                // The execution flow below won't be interrupted, because it's
                // sync.
                let unread_bytes =
                    self.header.as_ref().unwrap().len - self.bytes_read;
                let mut lim = buf.limit(unread_bytes);
                self.stream.try_read_buf(&mut lim)
            };
            match r {
                Ok(s) => {
                    if read_more_into_header {
                        assert!(s <= Window::BUF_LEN);
                        // buf gets written and written, it will eventually
                        // reach here
                        if self.hbuf.remaining_mut() == 0 {
                            self.header = Some(Window::deserialize(
                                &self.hbuf.get_ref()[..].try_into().unwrap(),
                            ));

                            self.hbuf.get_mut().clear();
                            // limit becomes 0. so reset it
                            self.hbuf.set_limit(Window::BUF_LEN);
                            // fully written
                        } else {
                            continue;
                        }
                    } else {
                        // more bytes read for this window
                        self.bytes_read += s;
                        if self.bytes_read == self.header.as_ref().unwrap().len
                        {
                            self.bytes_read = 0;
                            let w = self.header.take().unwrap();
                            return Poll::Ready(Ok(PollRes::WithAddr(w.addr)));
                        }
                    }
                }
                Err(e) => {
                    if matches!(e.kind(), io::ErrorKind::WouldBlock) {
                        continue;
                    } else {
                        return Poll::Ready(Err(e));
                    }
                }
            }
        }
    }
    /// must be called on init
    fn poll_signal(
        &self,
        cx: &mut std::task::Context<'_>, /* so this is in the call tree of a
                                          * Future */
    ) -> Poll<io::Result<usize>> {
        let mut b = BytesMut::new();
        b.put(&self.proto.to_be_bytes()[..]); // 8
        loop {
            ready!(self.stream.poll_write_ready(cx))?;

            match self.stream.try_write(&b) {
                Ok(s) => {
                    // TODO: should I assert this
                    assert!(s == b.len());
                    return Poll::Ready(Ok(s));
                }
                Err(e) => {
                    if matches!(e.kind(), io::ErrorKind::WouldBlock) {
                        continue;
                    } else {
                        return Poll::Ready(Err(e));
                    }
                }
            }
        }
    }
    /// await on this to get it inited
    pub fn init<'b>(&'b self) -> ProxySocketInit<'b, TYP> {
        ProxySocketInit { socket: self }
    }
}

pub trait Initable<'a> {
    type F: Future;
    fn init(&'a self) -> Self::F;
}

impl EmptyInit for TokioSocket {}
impl EmptyInit for SmolSocket {}
trait EmptyInit {}

impl<'a, T: EmptyInit> Initable<'a> for T {
    type F = Ready<()>;
    fn init(&self) -> Self::F {
        futures::future::ready(())
    }
}

impl<'a, const TYP: ProxySocketType> Initable<'a> for ProxySocket<'a, TYP> {
    type F = ProxySocketInit<'a, TYP>;
    fn init(&'a self) -> Self::F {
        ProxySocketInit { socket: self }
    }
}

pub struct ProxySocketInit<'a, const TYP: ProxySocketType> {
    socket: &'a ProxySocket<'a, TYP>,
}

impl<'a, const TYP: ProxySocketType> Future for ProxySocketInit<'a, TYP> {
    type Output = ();
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Self::Output> {
        self.socket.poll_signal(cx).map(|_| ())
    }
}

// Netlink socket is datagrams. Each read is proxied, with a header added.

enum PollRes {
    WithAddr(crate::SocketAddr), // may add more variant later ?
}

enum RequestType {
    With,
    Wthout,
    Proto,
}

/// Each time both packets are included in the window
#[derive(PartialEq, Eq, Debug)]
struct Window {
    addr: SocketAddr,
    /// denote the length of data after this struct
    len: usize,
}

// recvfrom and recvfrom_full work on the same 'channel'. So It's a relation of
// either. maybe signal next recv type when poll_write ?

impl Window {
    const BUF_LEN: usize = size_of::<SocketAddr>() + size_of::<usize>();
    fn deserialize(buf: &[u8; Self::BUF_LEN]) -> Self {
        let (l, r) = buf.split_at(size_of::<SocketAddr>());
        // try_into is entirely needless.
        let s = SocketAddr::deserialize(l.try_into().unwrap());
        let len = usize::from_be_bytes(r.try_into().unwrap());
        Self { addr: s, len }
    }
    fn serialize(&self) -> [u8; Self::BUF_LEN] {
        let mut b: [u8; Self::BUF_LEN] = [0; Self::BUF_LEN];
        b[..size_of::<SocketAddr>()].copy_from_slice(&self.addr.serialize());
        b[size_of::<SocketAddr>()..].copy_from_slice(&self.len.to_be_bytes());
        b
    }
}

#[test]
fn test_window() {
    // Test serialization and deserialization of a window with zeroed SocketAddr
    // and zero length
    let w1 = Window {
        addr: unsafe { std::mem::zeroed() },
        len: 0,
    };
    let b1 = w1.serialize();
    let w1_ = Window::deserialize(&b1);
    assert_eq!(w1, w1_);

    // Test serialization and deserialization of a window with a non-zeroed
    // SocketAddr and non-zero length
    let mut w2 = Window {
        addr: unsafe { std::mem::zeroed() },
        len: 0,
    };
    w2.addr.0.nl_family = 5;
    w2.addr.0.nl_pid = 6;
    w2.len = 10;

    let b2 = w2.serialize();
    let w2_ = Window::deserialize(&b2);
    assert_eq!(w2, w2_);

    // Test that two windows with different SocketAddr and same length are not
    // equal
    let mut w3 = Window {
        addr: unsafe { std::mem::zeroed() },
        len: 0,
    };
    w3.addr.0.nl_family = 5;
    w3.addr.0.nl_pid = 6;
    w3.len = 10;
    let mut w4 = Window {
        addr: unsafe { std::mem::zeroed() },
        len: 0,
    };
    w4.addr.0.nl_family = 7;
    w4.addr.0.nl_pid = 8;
    w4.len = 10;
    let b3 = w3.serialize();
    let b4 = w4.serialize();
    assert_ne!(b3, b4);
    let w3_ = Window::deserialize(&b3);
    let w4_ = Window::deserialize(&b4);
    assert_ne!(w3, w4);
    assert_ne!(w3_, w4_);

    // Test that two windows with same SocketAddr and different length are not
    // equal
    let mut w5 = Window {
        addr: unsafe { std::mem::zeroed() },
        len: 0,
    };
    w5.addr.0.nl_family = 5;
    w5.addr.0.nl_pid = 6;
    w5.len = 10;
    let mut w6 = Window {
        addr: unsafe { std::mem::zeroed() },
        len: 0,
    };
    w6.addr.0.nl_family = 5;
    w6.addr.0.nl_pid = 6;
    w6.len = 20;
    let b5 = w5.serialize();
    let b6 = w6.serialize();
    assert_ne!(b5, b6);
    let w5_ = Window::deserialize(&b5);
    let w6_ = Window::deserialize(&b6);
    assert_ne!(w5, w6);
    assert_ne!(w5_, w6_);
}

/// for the sake of laziness just have an alias
type Inode = u64;

/// with params
pub struct ProxyCtxP<'a> {
    pub shared: &'a mut ProxyCtx,
    /// Inode of self proxy socket
    pub inode: Inode,
}

#[derive(Debug)]
pub struct ProxyCtx {
    /// shared listener
    ul: UnixListener,
    /// This map should be filled out entirely on start, at least before the
    /// proxy sockets start
    pending: HashMap<Inode, UnixStream>,
    // let me just use RwLock not dashmap
}

impl ProxyCtx {
    pub fn new(path: PathBuf) -> std::io::Result<Self> {
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        Ok(Self {
            ul: UnixListener::bind(path)?,
            pending: HashMap::new(),
        })
    }
    /// get subs of num and quit
    pub async fn get_subs(&mut self, num: usize) -> std::io::Result<()> {
        while self.pending.len() < num {
            let (mut st, _addr) = self.ul.accept().await?;
            // addr will be anonymous, so no use. we don't use addr for id, just
            // send bytes.
            let id = st.read_u64().await?;
            self.pending.insert(id, st);
            // then the stream forwards netlink of that ns
        }
        Ok(())
    }
}

type ReqC = (BytesMut, Option<SocketAddr>);
type Res = (BytesMut, Window);

// wire
// read: [8, proto] [len | addr | buf]
// write: [ino] [window | buf]

// copied from netlink-proto/framed
pub const INITIAL_READER_CAPACITY: usize = 64 * 1024;
pub const INITIAL_WRITER_CAPACITY: usize = 8 * 1024;

pub async fn proxy<const TYP: ProxySocketType>(path: PathBuf) -> Result<()> {
    let mut upstr = UnixStream::connect(path).await?;

    let ino = get_inode_self_ns()?;
    upstr.write_u64(ino).await?;
    let mut base = [0; size_of::<isize>()];
    // Note: ReadBuf is better than BytesMut here which takes 64 bytes which is
    // too much. I can just use an array here.
    let _ = upstr.read_exact(&mut base).await?;
    let proto = isize::from_be_bytes(base.try_into().unwrap());
    let nl1 = TokioSocket::new(proto, ());
    let mut nl = nl1?;
    // messages to forward
    let (sx, mut rx) = unbounded_channel::<ReqC>();
    // messages to forward back
    let (sx2, mut rx2) = unbounded_channel::<Res>();
    let (mut up_r, mut up_w) = upstr.into_split();

    let netlink_t = async move {
        loop {
            let mut nlb = BytesMut::with_capacity(INITIAL_READER_CAPACITY);
            nlb.clear();
            let recv = async {
                match TYP {
                    ProxySocketType::PollRecvFrom => {
                        let r = nl.recv_from(&mut nlb).await?;
                        let w = Window {
                            addr: r,
                            len: nlb.len(),
                        };
                        sx2.send((nlb, w))?;
                    }
                    _ => unimplemented!(),
                }
                Result::<(), Box<dyn Error + Send + Sync>>::Ok(())
            };
            tokio::select! {
                r = rx.recv() => {
                    let x = r.unwrap();
                    let (b, s) = x;
                    match s {
                        Some(a) => {
                            nl.send_to(&b[..], &a).await?;
                        },
                        None => {
                            nl.send(&b).await?;
                        }
                    }
                },
                r = recv => r?
            }
        }
        Result::<(), Box<dyn Error + Send + Sync>>::Ok(())
    };

    let req_handle = async move {
        loop {
            let buflen: usize = up_r.read_u32().await?.try_into().unwrap(); // cast to usize
            let mut addr = [0; SocketAddr::LEN];
            up_r.read_exact(&mut addr).await?;
            let no_dst = is_all_zero(&addr);
            let mut buf = BytesMut::zeroed(buflen);
            up_r.read_exact(&mut buf[..buflen]).await?;
            if no_dst {
                sx.send((buf, None))?;
            } else {
                let pa = SocketAddr::deserialize(&addr);
                sx.send((buf, Some(pa)))?;
            }
        }

        Result::<(), Box<dyn Error + Send + Sync>>::Ok(())
    };

    let res_handle = async move {
        loop {
            let (buf, win) = rx2.recv().await.unwrap();
            up_w.write_all(&win.serialize()).await?;
            up_w.write_all(&buf).await?;
        }

        Result::<(), Box<dyn Error + Send + Sync>>::Ok(())
    };

    tokio::select! {
        _ = netlink_t => {}
        _ = req_handle => {}
        _ = res_handle => {}
    }

    Ok(())
}

pub fn is_all_zero(b: &[u8; 12]) -> bool {
    let mut r = true;
    for x in b {
        r &= *x == 0;
    }
    r
}

pub fn get_inode_self_ns() -> Result<u64> {
    let filename = CString::new("/proc/self/ns/net")?;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::stat(filename.as_ptr(), &mut st) };
    if result == -1 {
        let err = std::io::Error::last_os_error();
        anyhow::bail!(err);
    }
    let inode = st.st_ino;
    Ok(inode)
}
