use std::{
    collections::HashMap,
    convert::TryInto,
    ffi::CString,
    mem::{forget, size_of},
    os::fd::{AsFd, AsRawFd, FromRawFd},
    path::PathBuf,
    sync::Arc,
};
// use async_std::channel::unbounded;
use anyhow::{anyhow, Context, Result};
use tokio::sync::{oneshot, RwLock};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
};
use tokio_send_fd::SendFd;

use crate::{AsyncSocket, TokioSocket};
type Inode = u64;

pub struct NetlinkProxy {
    ul: UnixListener,
    pub pending: Arc<RwLock<HashMap<NProxyID, ProxyParam>>>,
}
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct NProxyID(pub Inode);

#[derive(Debug)]
pub struct ProxyParam {
    pub cb: oneshot::Sender<TokioSocket>,
    pub proto: isize,
}

impl NetlinkProxy {
    pub fn new(path: PathBuf) -> Result<Self> {
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        log::trace!("bind {:?}", &path);
        Ok(Self {
            ul: UnixListener::bind(path)?,
            pending: Default::default(),
        })
    }
    pub async fn serve(self) -> Result<()> {
        while let Ok((mut st, _addr)) = self.ul.accept().await {
            log::trace!("NetlinkProxy, new conn");
            let id = st.read_u64().await?;
            let mut g = self.pending.write().await;
            let param =
                g.remove(&NProxyID(id))
                    .ok_or(anyhow!(
                "programming error: no corresponding ProxyParam. ino {}", id
            ))?;
            let bytes = param.proto.to_be_bytes();
            st.write(&bytes).await?;
            let fd = st.recv_fd().await?;
            log::trace!("recved fd from proxy, ino {}", id);
            param
                .cb
                .send(unsafe { TokioSocket::from_raw_fd(fd) })
                .map_err(|_| anyhow!("send TokioSocket failed"))?;
        }
        Ok(())
    }
}

/// Proxy establishes a socket on behalf of the user
pub async fn proxy(path: PathBuf, mut id: Option<NProxyID>) -> Result<()> {
    let mut stream = UnixStream::connect(path).await?;

    if id.is_none() {
        id = Some(NProxyID(get_inode_self_ns()?));
    }

    stream.write_u64(id.unwrap().0).await?;
    let mut base = [0; size_of::<isize>()];
    _ = stream.read_exact(&mut base).await?;
    let proto = isize::from_be_bytes(base.try_into().unwrap());
    let nl = TokioSocket::new(proto)?;
    let fd = nl.as_raw_fd();
    stream.send_fd(fd).await?;
    // forget(nl);

    Ok(())
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
