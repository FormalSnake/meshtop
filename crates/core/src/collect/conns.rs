use netstat2::{AddressFamilyFlags, ProtocolFlags, ProtocolSocketInfo, TcpState};
use sysinfo::{Pid, System};

use crate::model::Conn;

/// Established TCP connections off loopback. The owning process is only
/// known for sockets this user can inspect; on Linux the socket owner's uid
/// is always known, which is enough to tell root's nix-daemon ssh apart from
/// an interactive one.
pub fn read(sys: &System) -> Vec<Conn> {
    let Ok(sockets) =
        netstat2::get_sockets_info(AddressFamilyFlags::IPV4 | AddressFamilyFlags::IPV6, ProtocolFlags::TCP)
    else {
        return Vec::new();
    };

    sockets
        .into_iter()
        .filter_map(|s| {
            let ProtocolSocketInfo::Tcp(tcp) = &s.protocol_socket_info else { return None };
            let remote = tcp.remote_addr.to_canonical();
            if tcp.state != TcpState::Established || remote.is_loopback() || remote.is_unspecified() {
                return None;
            }
            let process = s
                .associated_pids
                .first()
                .and_then(|pid| sys.process(Pid::from_u32(*pid)))
                .map(|p| p.name().to_string_lossy().to_string());
            #[cfg(target_os = "linux")]
            let uid = Some(s.uid);
            #[cfg(not(target_os = "linux"))]
            let uid = None;
            Some(Conn {
                local: tcp.local_addr.to_canonical(),
                local_port: tcp.local_port,
                remote,
                remote_port: tcp.remote_port,
                process,
                uid,
            })
        })
        .collect()
}
