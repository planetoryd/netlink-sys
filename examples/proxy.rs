use std::time::Duration;
use std::{error::Error, path::PathBuf, sync::Arc};

use bytes::BytesMut;
use libc::AF_INET;

use netlink_packet_core::{
    NetlinkBuffer, NetlinkMessage, NLM_F_DUMP, NLM_F_REQUEST,
};
use netlink_packet_route::{LinkHeader, RtnlMessage};
use netlink_sys::proxy::ProxySocketType;
use netlink_sys::AsyncSocketExt;
use netlink_sys::{
    protocols::NETLINK_ROUTE,
    proxy::{self, ProxyCtx, ProxyCtxP, ProxySocket},
    AsyncSocket, SocketAddr,
};
use anyhow::Result;

// proxy netlink through a unix socket
// we have a registry that keeps proxies on a per inode (netns file) basis

#[tokio::main]
async fn main() -> Result<()> {
    let p: PathBuf = "./p.sock".parse()?;
    let ctx = ProxyCtx::new(p.clone())?;
    let ser = tokio::spawn(hub(ctx));
    let prox =
        tokio::spawn(proxy::proxy::<{ ProxySocketType::PollRecvFrom }>(p));
    let (a, b) = tokio::try_join!(ser, prox)?;
    dbg!(a.err(), b.err());
    Ok(())
}

async fn hub(mut ctx: ProxyCtx) -> Result<()> {
    ctx.get_subs(1).await?;
    println!("collected subs");
    let kernel_unicast: SocketAddr = SocketAddr::new(0, 0);
    let mut params = ProxyCtxP {
        shared: &mut ctx,
        inode: proxy::get_inode_self_ns()?,
    };
    let mut pso = ProxySocket::new(NETLINK_ROUTE, &mut params)?;
    pso.init().await;
    let mut msg = netlink_packet_route::LinkMessage::default();
    msg.header.interface_family = AF_INET as u8;
    let mut nlmsg = NetlinkMessage::from(RtnlMessage::GetLink(msg));
    nlmsg.header.flags = NLM_F_REQUEST | NLM_F_DUMP;
    nlmsg.finalize();
    let mut buf = BytesMut::zeroed(nlmsg.buffer_len());
    nlmsg.serialize(&mut buf[..]);
    println!("send {} in len", nlmsg.buffer_len());
    pso.send_to(&buf[..nlmsg.buffer_len()], &kernel_unicast)
        .await?;

    loop {
        buf.clear();
        // each time a single datagram is received
        // truncated if buffer small
        // so we clear it each time
        println!("recv");
        let addr = pso.recv_from(&mut buf).await?;
        let blen = buf.len();
        {
            let mut nlbuf = NetlinkBuffer::new(&mut buf);
            nlbuf.set_length(blen as u32);
        }
        let parsed = NetlinkMessage::<RtnlMessage>::deserialize(&buf).unwrap();
        dbg!(&parsed);
    }

    println!("hub end");
    Ok(())
}
