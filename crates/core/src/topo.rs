use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;

use crate::config::Config;
use crate::model::Snapshot;

/// One TCP connection between two configured hosts, merged from whichever
/// ends reported it.
#[derive(Clone, Debug)]
pub struct Link {
    pub client: usize,
    pub server: usize,
    pub port: u16,
    pub kind: String,
    pub client_process: Option<String>,
}

#[derive(Default)]
pub struct Topology {
    pub links: Vec<Link>,
    /// Connections per host to addresses no configured host owns.
    pub external: Vec<usize>,
}

type Endpoint = (IpAddr, u16);

#[derive(Default)]
struct Seen {
    client: Option<(usize, Option<String>, Option<u32>)>,
    server: Option<(usize, Option<String>)>,
    port: u16,
}

pub fn build(snaps: &[Option<&Snapshot>], config: &Config) -> Topology {
    let mut owner: HashMap<IpAddr, Option<usize>> = HashMap::new();
    for (i, snap) in snaps.iter().enumerate() {
        for addr in snap.map(|s| s.addrs.as_slice()).unwrap_or_default() {
            // A docker bridge or VM subnet can repeat across machines; an
            // address claimed twice says nothing about who is on the other end.
            owner.entry(*addr).and_modify(|o| *o = o.filter(|&h| h == i)).or_insert(Some(i));
        }
    }

    let mut external = vec![0; snaps.len()];
    let mut seen: BTreeMap<(Endpoint, Endpoint), Seen> = BTreeMap::new();

    for (h, snap) in snaps.iter().enumerate() {
        let Some(snap) = snap else { continue };
        for c in &snap.conns {
            let Some(Some(peer)) = owner.get(&c.remote).copied() else {
                external[h] += 1;
                continue;
            };
            if peer == h {
                continue;
            }
            let ours = (c.local, c.local_port);
            let theirs = (c.remote, c.remote_port);
            let key = if ours < theirs { (ours, theirs) } else { (theirs, ours) };
            let entry = seen.entry(key).or_default();
            // The listening side is the one on the lower port; the other end
            // is an ephemeral port picked by the client.
            // Equal ports (syncthing dials 22000 from 22000) fall back to
            // the address order, so both ends agree on who is the client.
            if (c.remote_port, c.remote) < (c.local_port, c.local) {
                entry.port = c.remote_port;
                entry.client = Some((h, c.process.clone(), c.uid));
                entry.server.get_or_insert((peer, None));
            } else {
                entry.port = c.local_port;
                entry.server = Some((h, c.process.clone()));
                entry.client.get_or_insert((peer, None, None));
            }
        }
    }

    let links = seen
        .into_values()
        .filter_map(|s| {
            let (client, client_process, uid) = s.client?;
            let (server, server_process) = s.server?;
            let kind = kind(config, s.port, client_process.as_deref(), uid, server_process.as_deref());
            Some(Link { client, server, port: s.port, kind, client_process })
        })
        .collect();

    Topology { links, external }
}

fn kind(config: &Config, port: u16, client: Option<&str>, uid: Option<u32>, server: Option<&str>) -> String {
    if let Some(label) = config.port_label(port) {
        return label.into();
    }
    let label = match port {
        // Remote builds are ssh from the root nix-daemon; on Linux the socket
        // uid gives that away even when the process is hidden from us.
        22 if client == Some("nix-daemon") || (client.is_none() && uid == Some(0)) => "nix build",
        22 => "ssh",
        22000 => "syncthing",
        3389 => "rdp",
        5900..=5909 => "vnc",
        47984 | 47989 | 48010 => "sunshine",
        9418 => "git",
        80 | 443 | 8080 | 8443 => "http",
        _ => return server.or(client).map(str::to_string).unwrap_or_else(|| format!(":{port}")),
    };
    label.into()
}

impl Topology {
    /// Links grouped by (client, server), each with its kinds and counts.
    pub fn edges(&self) -> BTreeMap<(usize, usize), BTreeMap<String, usize>> {
        let mut edges: BTreeMap<(usize, usize), BTreeMap<String, usize>> = BTreeMap::new();
        for l in &self.links {
            *edges.entry((l.client, l.server)).or_default().entry(l.kind.clone()).or_default() += 1;
        }
        edges
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Conn;

    fn snap(addr: &str, conns: Vec<Conn>) -> Snapshot {
        Snapshot { addrs: vec![addr.parse().unwrap()], conns, ..Default::default() }
    }

    fn conn(local: (&str, u16), remote: (&str, u16), process: Option<&str>, uid: Option<u32>) -> Conn {
        Conn {
            local: local.0.parse().unwrap(),
            local_port: local.1,
            remote: remote.0.parse().unwrap(),
            remote_port: remote.1,
            process: process.map(Into::into),
            uid,
        }
    }

    #[test]
    fn both_ends_merge_into_one_link() {
        let (mac, g815) = ("100.75.60.102", "100.114.32.78");
        let snaps = [
            snap(g815, vec![conn((g815, 46052), (mac, 22), Some("ssh"), Some(1000))]),
            snap(mac, vec![conn((mac, 22), (g815, 46052), None, None)]),
        ];
        let t = build(&[Some(&snaps[0]), Some(&snaps[1])], &Config::default());
        assert_eq!(t.links.len(), 1);
        let l = &t.links[0];
        assert_eq!((l.client, l.server, l.port, l.kind.as_str()), (0, 1, 22, "ssh"));
        assert_eq!(l.client_process.as_deref(), Some("ssh"));
    }

    #[test]
    fn equal_ports_never_make_a_self_link() {
        let (g, e) = ("100.114.32.78", "100.109.196.64");
        let snaps = [
            snap(g, vec![conn((g, 22000), (e, 22000), None, None)]),
            snap(e, vec![conn((e, 22000), (g, 22000), None, None)]),
        ];
        let t = build(&[Some(&snaps[0]), Some(&snaps[1])], &Config::default());
        assert_eq!(t.links.len(), 1);
        assert_ne!(t.links[0].client, t.links[0].server);
        assert_eq!(t.links[0].kind, "syncthing");
    }

    #[test]
    fn root_ssh_is_a_nix_build() {
        let (e, g) = ("100.109.196.64", "100.114.32.78");
        let snaps = [snap(e, vec![conn((e, 34682), (g, 22), None, Some(0))]), snap(g, vec![])];
        let t = build(&[Some(&snaps[0]), Some(&snaps[1])], &Config::default());
        assert_eq!(t.links[0].kind, "nix build");
    }

    #[test]
    fn unknown_peers_count_as_external() {
        let a = "100.75.60.102";
        let snaps = [snap(a, vec![conn((a, 50000), ("1.1.1.1", 443), Some("curl"), None)])];
        let t = build(&[Some(&snaps[0])], &Config::default());
        assert!(t.links.is_empty());
        assert_eq!(t.external, vec![1]);
    }

    #[test]
    fn configured_port_labels_win() {
        let (a, b) = ("100.75.60.102", "100.114.32.78");
        let mut config = Config::default();
        config.ports.insert("1234".into(), "bluebubbles".into());
        let snaps = [snap(a, vec![]), snap(b, vec![conn((b, 51232), (a, 1234), Some("messages"), None)])];
        let t = build(&[Some(&snaps[0]), Some(&snaps[1])], &config);
        assert_eq!((t.links[0].client, t.links[0].kind.as_str()), (1, "bluebubbles"));
    }
}
