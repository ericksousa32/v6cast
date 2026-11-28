//! IPv6 global address discovery and change monitoring.
//!
//! The endpoint a viewer connects to is the stable global unicast address (GUA)
//! of the active interface. Selection rules ([`select_gua`]):
//!
//! 1. Only global unicast addresses (`2000::/3`) qualify. Link-local
//!    (`fe80::/10`), unique local (`fc00::/7`), loopback, multicast,
//!    documentation (`2001:db8::/32`), Teredo (`2001::/32`) and 6to4
//!    (`2002::/16`) addresses are skipped, as are addresses still in, or
//!    failed by, duplicate address detection and addresses on interfaces that
//!    are down.
//! 2. If the active interface (the one the OS would use to reach the IPv6
//!    internet) has a usable address, only its addresses are considered;
//!    otherwise every interface is.
//! 3. Stable addresses win over temporary (RFC 4941 privacy) addresses, and
//!    non-deprecated over deprecated ones. A temporary address is only used when
//!    no stable one exists.
//! 4. Among equals the previously published address is kept, so the endpoint
//!    does not flap; otherwise the lowest address wins, for determinism.
//!
//! Addresses and their flags come from the `netdev` crate (netlink on Linux,
//! `GetAdaptersAddresses` on Windows), so this module has no platform-specific
//! code. [`EndpointMonitor`] re-reads them periodically and publishes the new
//! endpoint when the prefix or interface changes.

use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Category of an IPv6 address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressScope {
    Unspecified,
    Loopback,
    Multicast,
    LinkLocal,
    UniqueLocal,
    Documentation,
    Teredo,
    SixToFour,
    Ipv4Mapped,
    /// Global unicast, eligible for publishing.
    Global,
    /// Anything else (reserved or deprecated ranges such as site-local).
    Other,
}

/// Classifies an address by prefix.
pub fn classify(addr: Ipv6Addr) -> AddressScope {
    let s = addr.segments();
    if addr.is_unspecified() {
        AddressScope::Unspecified
    } else if addr.is_loopback() {
        AddressScope::Loopback
    } else if s[0] & 0xff00 == 0xff00 {
        AddressScope::Multicast
    } else if s[0] & 0xffc0 == 0xfe80 {
        AddressScope::LinkLocal
    } else if s[0] & 0xfe00 == 0xfc00 {
        AddressScope::UniqueLocal
    } else if s[..6] == [0, 0, 0, 0, 0, 0xffff] {
        AddressScope::Ipv4Mapped
    } else if s[0] == 0x2001 && s[1] == 0x0db8 {
        AddressScope::Documentation
    } else if s[0] == 0x2001 && s[1] == 0 {
        AddressScope::Teredo
    } else if s[0] == 0x2002 {
        AddressScope::SixToFour
    } else if s[0] & 0xe000 == 0x2000 {
        AddressScope::Global
    } else {
        AddressScope::Other
    }
}

/// One IPv6 address as the operating system reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressCandidate {
    pub addr: Ipv6Addr,
    pub interface_index: u32,
    pub interface_name: String,
    pub interface_up: bool,
    /// RFC 4941 privacy address, short-lived.
    pub temporary: bool,
    /// Preferred lifetime expired.
    pub deprecated: bool,
    /// Duplicate address detection still running.
    pub tentative: bool,
    /// Duplicate address detection failed.
    pub duplicated: bool,
}

impl AddressCandidate {
    fn usable(&self) -> bool {
        classify(self.addr) == AddressScope::Global
            && self.interface_up
            && !self.tentative
            && !self.duplicated
    }

    /// Lower is better: stable before temporary, current before deprecated.
    fn rank(&self) -> u8 {
        u8::from(self.deprecated) * 2 + u8::from(self.temporary)
    }
}

/// Addresses and active interface at one point in time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub candidates: Vec<AddressCandidate>,
    /// Index of the interface used for the IPv6 default route, if known.
    pub active_interface: Option<u32>,
}

/// Picks the address to publish. See the module docs for the rules.
pub fn select_gua(snapshot: &Snapshot, previous: Option<Ipv6Addr>) -> Option<&AddressCandidate> {
    let usable: Vec<&AddressCandidate> =
        snapshot.candidates.iter().filter(|c| c.usable()).collect();
    let on_active: Vec<&AddressCandidate> = match snapshot.active_interface {
        Some(index) => usable
            .iter()
            .copied()
            .filter(|c| c.interface_index == index)
            .collect(),
        None => Vec::new(),
    };
    let pool = if on_active.is_empty() {
        usable
    } else {
        on_active
    };
    let best_rank = pool.iter().map(|c| c.rank()).min()?;
    let best = pool.into_iter().filter(|c| c.rank() == best_rank);
    let best: Vec<&AddressCandidate> = best.collect();
    if let Some(prev) = previous {
        if let Some(kept) = best.iter().find(|c| c.addr == prev) {
            return Some(kept);
        }
    }
    best.into_iter().min_by_key(|c| c.addr)
}

/// Reads the addresses of every interface from the operating system.
pub fn system_snapshot() -> Snapshot {
    let interfaces = netdev::get_interfaces();
    let mut candidates = Vec::new();
    for iface in &interfaces {
        for (i, net) in iface.ipv6.iter().enumerate() {
            let flags = iface.ipv6_addr_flags.get(i).copied().unwrap_or_default();
            candidates.push(AddressCandidate {
                addr: net.addr(),
                interface_index: iface.index,
                interface_name: iface.name.clone(),
                interface_up: iface.is_up(),
                temporary: flags.temporary,
                deprecated: flags.deprecated,
                tentative: flags.tentative,
                duplicated: flags.duplicated,
            });
        }
    }
    let active_interface =
        route_probe(&candidates).or_else(|| netdev::get_default_interface().ok().map(|i| i.index));
    Snapshot {
        candidates,
        active_interface,
    }
}

/// Asks the OS which source address it would use to reach the IPv6 internet.
///
/// Connecting a UDP socket sends no packet; it only runs route selection.
fn route_probe(candidates: &[AddressCandidate]) -> Option<u32> {
    const PROBE: SocketAddrV6 = SocketAddrV6::new(
        Ipv6Addr::new(0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8888),
        443,
        0,
        0,
    );
    let socket = UdpSocket::bind(SocketAddr::V6(SocketAddrV6::new(
        Ipv6Addr::UNSPECIFIED,
        0,
        0,
        0,
    )))
    .ok()?;
    socket.connect(SocketAddr::V6(PROBE)).ok()?;
    let SocketAddr::V6(local) = socket.local_addr().ok()? else {
        return None;
    };
    candidates
        .iter()
        .find(|c| c.addr == *local.ip())
        .map(|c| c.interface_index)
}

/// Address to publish to the group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicEndpoint {
    pub addr: SocketAddrV6,
    pub interface_name: String,
    /// True when no stable address existed and a privacy address was chosen.
    pub temporary: bool,
}

type SnapshotSource = Arc<dyn Fn() -> Snapshot + Send + Sync>;

/// Watches the addresses and publishes the endpoint whenever it changes.
#[derive(Debug)]
pub struct EndpointMonitor {
    rx: watch::Receiver<Option<PublicEndpoint>>,
    task: JoinHandle<()>,
}

impl EndpointMonitor {
    /// Default polling interval.
    pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(5);

    /// Monitors the system addresses for the server listening on `port`.
    /// Must run inside a Tokio runtime.
    pub fn spawn(port: u16, interval: Duration) -> Self {
        Self::spawn_with(port, interval, Arc::new(system_snapshot))
    }

    /// Same as [`EndpointMonitor::spawn`] with another address source (tests).
    pub fn spawn_with(port: u16, interval: Duration, source: SnapshotSource) -> Self {
        let (tx, rx) = watch::channel(None);
        let task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                let source = source.clone();
                // netlink and GetAdaptersAddresses are blocking calls.
                let Ok(snapshot) = tokio::task::spawn_blocking(move || source()).await else {
                    continue;
                };
                tx.send_if_modified(|current: &mut Option<PublicEndpoint>| {
                    let previous = current.as_ref().map(|e| *e.addr.ip());
                    let next = select_gua(&snapshot, previous).map(|c| PublicEndpoint {
                        addr: SocketAddrV6::new(c.addr, port, 0, 0),
                        interface_name: c.interface_name.clone(),
                        temporary: c.temporary,
                    });
                    if *current == next {
                        return false;
                    }
                    tracing::info!(endpoint = ?next, "published IPv6 endpoint changed");
                    *current = next;
                    true
                });
                if tx.is_closed() {
                    break;
                }
            }
        });
        Self { rx, task }
    }

    /// Endpoint currently published, if any.
    pub fn current(&self) -> Option<PublicEndpoint> {
        self.rx.borrow().clone()
    }

    /// Receiver that is notified on every change.
    pub fn subscribe(&self) -> watch::Receiver<Option<PublicEndpoint>> {
        self.rx.clone()
    }
}

impl Drop for EndpointMonitor {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    fn ip(s: &str) -> Ipv6Addr {
        s.parse().unwrap()
    }

    fn cand(addr: &str, iface: u32) -> AddressCandidate {
        AddressCandidate {
            addr: ip(addr),
            interface_index: iface,
            interface_name: format!("if{iface}"),
            interface_up: true,
            temporary: false,
            deprecated: false,
            tentative: false,
            duplicated: false,
        }
    }

    fn snap(candidates: Vec<AddressCandidate>, active: Option<u32>) -> Snapshot {
        Snapshot {
            candidates,
            active_interface: active,
        }
    }

    fn pick(s: &Snapshot) -> Option<Ipv6Addr> {
        select_gua(s, None).map(|c| c.addr)
    }

    #[test]
    fn classifies_every_range() {
        let cases = [
            ("::", AddressScope::Unspecified),
            ("::1", AddressScope::Loopback),
            ("ff02::1", AddressScope::Multicast),
            ("fe80::1", AddressScope::LinkLocal),
            ("febf:ffff::1", AddressScope::LinkLocal),
            ("fc00::1", AddressScope::UniqueLocal),
            ("fd12:3456::1", AddressScope::UniqueLocal),
            ("::ffff:192.0.2.1", AddressScope::Ipv4Mapped),
            ("2001:db8::1", AddressScope::Documentation),
            ("2001:0:4136:e378::1", AddressScope::Teredo),
            ("2002:c000:201::1", AddressScope::SixToFour),
            ("2804:14c:65::1", AddressScope::Global),
            ("2a02:8070::1", AddressScope::Global),
            ("3fff:ffff::1", AddressScope::Global),
            ("fec0::1", AddressScope::Other),
            ("4000::1", AddressScope::Other),
        ];
        for (addr, scope) in cases {
            assert_eq!(classify(ip(addr)), scope, "{addr}");
        }
    }

    #[test]
    fn non_global_addresses_are_never_published() {
        let s = snap(
            vec![
                cand("fe80::1", 1),
                cand("fd00::1", 1),
                cand("::1", 1),
                cand("2001:db8::1", 1),
                cand("2001:0:4136:e378::1", 1),
                cand("2002:c000:201::1", 1),
            ],
            Some(1),
        );
        assert_eq!(pick(&s), None);
    }

    #[test]
    fn stable_address_beats_temporary() {
        let mut temp = cand("2804:14c::aaaa", 1);
        temp.temporary = true;
        let s = snap(vec![temp.clone(), cand("2804:14c::ffff", 1)], Some(1));
        assert_eq!(pick(&s), Some(ip("2804:14c::ffff")));
        // Without a stable address the temporary one is still better than nothing.
        let s = snap(vec![temp], Some(1));
        let chosen = select_gua(&s, None).unwrap();
        assert!(chosen.temporary);
    }

    #[test]
    fn deprecated_addresses_come_last() {
        let mut old = cand("2804:14c::1", 1);
        old.deprecated = true;
        let mut temp = cand("2804:14c::2", 1);
        temp.temporary = true;
        let s = snap(vec![old.clone(), temp], Some(1));
        assert_eq!(pick(&s), Some(ip("2804:14c::2")));
        assert_eq!(pick(&snap(vec![old], Some(1))), Some(ip("2804:14c::1")));
    }

    #[test]
    fn tentative_duplicated_and_down_are_skipped() {
        let mut tentative = cand("2804:14c::1", 1);
        tentative.tentative = true;
        let mut dup = cand("2804:14c::2", 1);
        dup.duplicated = true;
        let mut down = cand("2804:14c::3", 1);
        down.interface_up = false;
        assert_eq!(pick(&snap(vec![tentative, dup, down], Some(1))), None);
    }

    #[test]
    fn active_interface_wins_and_others_are_a_fallback() {
        let s = snap(
            vec![cand("2804:14c::1", 1), cand("2a02:8070::9", 2)],
            Some(2),
        );
        assert_eq!(pick(&s), Some(ip("2a02:8070::9")));
        // Active interface without a GUA (e.g. only link-local): use another one.
        let s = snap(vec![cand("fe80::1", 3), cand("2804:14c::1", 1)], Some(3));
        assert_eq!(pick(&s), Some(ip("2804:14c::1")));
        // Unknown active interface: every interface counts.
        let s = snap(vec![cand("2a02:8070::9", 2), cand("2804:14c::1", 1)], None);
        assert_eq!(pick(&s), Some(ip("2804:14c::1")));
    }

    #[test]
    fn previous_endpoint_is_kept_while_still_best() {
        let s = snap(
            vec![cand("2804:14c::1", 1), cand("2804:14c::9", 1)],
            Some(1),
        );
        assert_eq!(pick(&s), Some(ip("2804:14c::1")));
        let kept = select_gua(&s, Some(ip("2804:14c::9"))).unwrap();
        assert_eq!(kept.addr, ip("2804:14c::9"));
        // A previous address that lost its rank is replaced.
        let mut s2 = s.clone();
        s2.candidates[1].temporary = true;
        let chosen = select_gua(&s2, Some(ip("2804:14c::9"))).unwrap();
        assert_eq!(chosen.addr, ip("2804:14c::1"));
    }

    #[test]
    fn system_snapshot_does_not_panic() {
        // Runs on the CI runners too, which may have no global IPv6.
        let s = system_snapshot();
        if let Some(c) = select_gua(&s, None) {
            assert_eq!(classify(c.addr), AddressScope::Global);
        }
    }

    #[tokio::test]
    async fn monitor_publishes_prefix_changes_only() {
        let state = Arc::new(Mutex::new(snap(vec![cand("2804:14c:1::1", 1)], Some(1))));
        let source_state = state.clone();
        let monitor = EndpointMonitor::spawn_with(
            4433,
            Duration::from_millis(10),
            Arc::new(move || source_state.lock().unwrap().clone()),
        );
        let mut rx = monitor.subscribe();
        let wait = |rx: &mut watch::Receiver<Option<PublicEndpoint>>| {
            let mut rx = rx.clone();
            async move {
                tokio::time::timeout(Duration::from_secs(5), rx.changed())
                    .await
                    .expect("no endpoint change published")
                    .unwrap();
                rx.borrow_and_update().clone()
            }
        };
        let first = wait(&mut rx).await.unwrap();
        rx.mark_unchanged();
        assert_eq!(first.addr, "[2804:14c:1::1]:4433".parse().unwrap());
        assert_eq!(monitor.current(), Some(first.clone()));

        // Same addresses for a while: nothing new is published.
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(!rx.has_changed().unwrap());

        // The ISP hands out a new prefix.
        *state.lock().unwrap() = snap(vec![cand("2804:14c:2::1", 1)], Some(1));
        let second = wait(&mut rx).await.unwrap();
        assert_eq!(second.addr, "[2804:14c:2::1]:4433".parse().unwrap());

        // Connectivity lost: the endpoint is withdrawn.
        *state.lock().unwrap() = snap(vec![cand("fe80::1", 1)], Some(1));
        rx.mark_unchanged();
        let mut rx2 = rx.clone();
        tokio::time::timeout(Duration::from_secs(5), rx2.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(monitor.current(), None);
    }
}
