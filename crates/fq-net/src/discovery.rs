//! UDP 发现端点:通告的发送与接收。
//!
//! 通告同时发往:
//! 1. **广播地址**(`255.255.255.255:24250`,真实局域网的主路径)
//! 2. **bootstrap 地址列表**(手动配置的对端发现地址 —— 单播直达,
//!    用于企业网禁用广播的退路,以及同机双进程测试的确定性路径)
//!
//! 一个 UDP 数据报 = 一帧(4 字节长度前缀 + MessagePack 报文),与 TCP 帧格式
//! 完全一致,编解码代码零分叉。
//!
//! 套接字选项:
//! * `SO_REUSEADDR` —— 同机多实例共享发现端口(跨机本来就不冲突);
//!   Windows 上多套接字绑同一端口时,**广播**会投递给全部实例(正是我们想要的),
//!   单播投递给其中一个(所以同机测试走 bootstrap 显式互指,不依赖单播随机性)
//! * `SO_BROADCAST` —— 允许向 255.255.255.255 发送

use std::net::SocketAddr;
use std::sync::Arc;

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;

use crate::error::Result;

/// Windows 专用:SIO_UDP_CONNRESET = 0(禁止 ICMP 错误上报到 recv_from)。
///
/// 背景:UDP socket 发往一个不可达端口后,Windows 会把返回的 ICMP
/// port-unreachable 转成 pending 的 `WSAECONNRESET`,下一次 `recv_from`
/// 返回错误而不是数据 —— 若通告周期性发往死地址(企业网禁广播时的
/// 255.255.255.255 即如此),接收循环会被错误风暴淹没,真实数据报饿死
/// (os error 10054 每 150ms 一次)。禁用上报是 Windows UDP 服务的标准做法。
///
/// 安全性说明:unsafe 边界仅为一次 `WSAIoctl` 系统调用,参数全部显式
/// 构造、无指针逃逸;失败时仅记录日志,不影响收发主流程。
#[cfg(windows)]
#[allow(unsafe_code)]
fn suppress_connreset(socket: &Socket) {
    use std::os::windows::io::AsRawSocket;
    use std::ptr;
    use windows_sys::Win32::Networking::WinSock::WSAIoctl;
    // IOC_IN(0x80000000) | IOC_VENDOR(0x18000000) | 12
    const SIO_UDP_CONNRESET: u32 = 0x9800_000C;
    let disable: u32 = 0; // FALSE = 不上报
    let mut returned: u32 = 0;
    let result = unsafe {
        WSAIoctl(
            socket.as_raw_socket() as usize,
            SIO_UDP_CONNRESET,
            ptr::from_ref(&disable).cast(),
            std::mem::size_of::<u32>() as u32,
            ptr::null_mut(),
            0,
            &mut returned,
            ptr::null_mut(),
            None, // 完成回调:同步调用不用
        )
    };
    if result != 0 {
        tracing::warn!(
            target = "fq_net::discovery",
            "SIO_UDP_CONNRESET 设置失败,接收循环可能受错误风暴干扰"
        );
    }
}

#[cfg(not(windows))]
fn suppress_connreset(_socket: &Socket) {}

/// 枚举本机全部可用的 IPv4 地址(跳过回环/链路本地/未指定)。
///
/// 多网卡机器会有多个地址;UI 展示与定向广播都用它。
pub fn local_ipv4_addresses() -> Vec<std::net::Ipv4Addr> {
    let Ok(interfaces) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    interfaces
        .into_iter()
        .filter_map(|interface| match interface.ip() {
            std::net::IpAddr::V4(ip) => Some(ip),
            std::net::IpAddr::V6(_) => None,
        })
        .filter(|ip| !(ip.is_loopback() || ip.is_link_local() || ip.is_unspecified()))
        .collect()
}

/// 为每块 IPv4 网卡创建一个绑定该网卡本机 IP 的广播发送端。
///
/// 绑定了本机 IP 的 socket 发送 255.255.255.255 时,操作系统会从**该网卡**发出
/// —— 这是多网卡场景下"广播覆盖所有网段"的标准做法(默认路由只走一块网卡)。
fn build_interface_senders() -> Vec<Arc<UdpSocket>> {
    let mut senders = Vec::new();
    for ip in local_ipv4_addresses() {
        // 两步都可能失败:socket2 构建/绑定,以及 tokio 注册 reactor
        let socket = match sender_for(ip).and_then(UdpSocket::from_std) {
            Ok(socket) => socket,
            Err(e) => {
                tracing::debug!(target = "fq_net::discovery", %e, %ip, "网卡发送端创建失败");
                continue;
            }
        };
        tracing::debug!(target = "fq_net::discovery", %ip, "网卡广播发送端就绪");
        senders.push(Arc::new(socket));
    }
    senders
}

/// 绑定指定网卡 IP 的广播发送端(端口随机)。
fn sender_for(ip: std::net::Ipv4Addr) -> std::io::Result<std::net::UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_broadcast(true)?;
    socket.bind(&socket2::SockAddr::from(SocketAddr::from((ip, 0))))?;
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

/// UDP 发现端点。
///
/// 通告的发送路径(**多网卡必须全走**,否则广播只从默认路由网卡出去,
/// 同伴在其它网卡/网段时永远互相看不见):
/// 1. 主 socket(绑定 0.0.0.0)发往 `255.255.255.255` —— 走默认路由
/// 2. **每块网卡的定向发送 socket**(绑定该网卡的本机 IP)各发一次广播 ——
///    绑定本机 IP 的 socket 发广播,Windows 会从对应网卡发出
/// 3. bootstrap 单播列表 —— 禁广播网络的确定性退路
#[derive(Debug)]
pub struct DiscoveryEndpoint {
    socket: Arc<UdpSocket>,
    /// 每网卡一个定向广播发送端。
    interface_senders: Vec<Arc<UdpSocket>>,
    broadcast: SocketAddr,
    bootstrap: Vec<SocketAddr>,
}

impl DiscoveryEndpoint {
    /// 绑定发现端口。
    ///
    /// `bind` 通常为 `0.0.0.0:24250`;`broadcast` 为广播目标;`bootstrap` 为
    /// 需要单播通告的对端发现地址列表(可为空)。
    pub fn bind(
        bind: SocketAddr,
        broadcast: SocketAddr,
        bootstrap: Vec<SocketAddr>,
    ) -> std::io::Result<Self> {
        let domain = if bind.is_ipv4() {
            Domain::IPV4
        } else {
            Domain::IPV6
        };
        let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
        socket.set_reuse_address(true)?;
        socket.set_broadcast(true)?;
        // Windows:抑制 ICMP port-unreachable 变成的 WSAECONNRESET 上报。
        // 不这么做,任何一次发往不可达端口的 UDP(广播被禁的企业网很常见)
        // 都会让后续 recv_from 持续报错,数据报被错误风暴饿死。
        suppress_connreset(&socket);
        socket.bind(&socket2::SockAddr::from(bind))?;
        socket.set_nonblocking(true)?;
        let socket = Arc::new(UdpSocket::from_std(socket.into())?);

        let interface_senders = build_interface_senders();
        tracing::info!(
            target = "fq_net::discovery",
            local = %socket.local_addr().map(|a| a.to_string()).unwrap_or_default(),
            interfaces = interface_senders.len(),
            "发现端点就绪(每网卡一个广播发送端)"
        );

        Ok(Self {
            socket,
            interface_senders,
            broadcast,
            bootstrap,
        })
    }

    /// 实际绑定的本地地址(端口填 0 时用于回查)。
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// 把一个通告数据报发往所有发现路径(默认路由广播 + 每网卡广播 + bootstrap)。
    ///
    /// 单个目标失败只记日志不上抛 —— 任何一条路径可达,发现就能成立。
    pub async fn announce(&self, datagram: &[u8]) {
        if let Err(e) = self.socket.send_to(datagram, self.broadcast).await {
            tracing::debug!(target = "fq_net::discovery", %e, addr = %self.broadcast, "默认路由广播发送失败");
        }
        for sender in &self.interface_senders {
            if let Err(e) = sender.send_to(datagram, self.broadcast).await {
                tracing::debug!(target = "fq_net::discovery", %e, addr = %self.broadcast, "网卡广播发送失败");
            }
        }
        for target in &self.bootstrap {
            if let Err(e) = self.socket.send_to(datagram, target).await {
                tracing::debug!(target = "fq_net::discovery", %e, addr = %target, "bootstrap 通告发送失败");
            }
        }
    }

    /// 向指定地址**单播**一份通告副本(手动探测 / 直连握手用)。
    pub async fn announce_to(&self, datagram: &[u8], target: SocketAddr) {
        if let Err(e) = self.socket.send_to(datagram, target).await {
            tracing::debug!(target = "fq_net::discovery", %e, %target, "定向通告发送失败");
        }
    }

    /// 接收一个数据报,返回(字节数, 来源地址)。
    pub async fn recv_from(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr)> {
        let (size, source) = self.socket.recv_from(buf).await?;
        Ok((size, source))
    }

    /// 底层套接字(事件循环共享用)。
    pub fn socket(&self) -> Arc<UdpSocket> {
        Arc::clone(&self.socket)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn tokio_rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn bootstrap_unicast_roundtrip_on_loopback() {
        tokio_rt().block_on(async {
            // A 只用回环地址作"广播目标"(测试不触碰真实网卡);
            // B 从 bootstrap 单播路径收到 A 的通告
            let a = DiscoveryEndpoint::bind(
                SocketAddr::from(([127, 0, 0, 1], 0)),
                SocketAddr::from(([127, 0, 0, 1], 1)), // 不可达目标,失败仅告警
                vec![],
            )
            .unwrap();
            let a_addr = a.local_addr().unwrap();

            let b = DiscoveryEndpoint::bind(
                SocketAddr::from(([127, 0, 0, 1], 0)),
                SocketAddr::from(([127, 0, 0, 1], 1)),
                vec![a_addr],
            )
            .unwrap();

            let payload = b"announce-frame";
            b.announce(payload).await;

            let mut buf = [0u8; 1500];
            let (size, source) =
                tokio::time::timeout(std::time::Duration::from_secs(2), a.recv_from(&mut buf))
                    .await
                    .expect("超时")
                    .unwrap();
            assert_eq!(&buf[..size], payload);
            assert_eq!(source, b.local_addr().unwrap());
        });
    }

    #[test]
    fn same_port_double_bind_is_allowed_for_multi_instance() {
        // from_std 需要活跃的 tokio reactor
        tokio_rt().block_on(async {
            // 同机双实例的关键前提:REUSEADDR 下两个套接字可绑同一端口
            let bind = SocketAddr::from(([127, 0, 0, 1], 0));
            let first = DiscoveryEndpoint::bind(bind, bind, vec![]).unwrap();
            let first_addr = first.local_addr().unwrap();
            let second = DiscoveryEndpoint::bind(first_addr, first_addr, vec![]);
            assert!(second.is_ok(), "同端口二次绑定必须成功: {second:?}");
        });
    }
}
