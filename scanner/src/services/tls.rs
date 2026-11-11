//! The unit's certificate for HTTPS, made by the unit: a CA of its own, whose key stays on the
//! flash, and a certificate signed by it for the names the unit is reached by now (its addresses,
//! its host name, names its administrator added). The certificate is made again when those names
//! change or it nears its end; the CA is made once, so a PC that installed it goes on trusting
//! the unit.
//!
//! The CA can only sign for private addresses and local names (its name constraints): its key on
//! a board is worth little to whoever takes it. A name or an address outside them is left out of
//! the certificate and reported, and so is a name that is not a host name as a certificate
//! writes one: a verifier that reads the names strictly refuses the whole certificate for it.
//!
//! The board has no clock of its own. A certificate made while the clock is wrong (1970) is made
//! again once it is right.

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use aws_lc_rs::digest;
use rcgen::{
    date_time_ymd, BasicConstraints, Certificate, CertificateParams, CidrSubnet, DistinguishedName, DnType, ExtendedKeyUsagePurpose, GeneralSubtree, IsCa, KeyPair, KeyUsagePurpose,
    NameConstraints, PKCS_ECDSA_P256_SHA256,
};
use serde::{Deserialize, Serialize};

use crate::util::atomic_file;
use crate::util::time::utc_fields;

const DAY_S: u64 = 86_400;
/// A certificate lasts this long (Apple's browsers refuse one of more than 825 days).
const LASTS_DAYS: u64 = 820;
/// It is made again this long before its end.
const RENEW_DAYS: u64 = 30;
/// A clock before this (2026-01-01) is not set yet.
const CLOCK_FLOOR_S: u64 = 1_767_225_600;
/// The local names the CA may sign for, with each one's sub-names, besides the unit's host name.
const LOCAL_DOMAINS: [&str; 5] = ["localhost", "local", "lan", "home.arpa", "internal"];
/// The private addresses the CA may sign for: loopback, link-local, the private networks, and
/// the shared range a VPN gives (100.64/10).
const PRIVATE_V4: [([u8; 4], u8); 6] = [([127, 0, 0, 0], 8), ([10, 0, 0, 0], 8), ([172, 16, 0, 0], 12), ([192, 168, 0, 0], 16), ([169, 254, 0, 0], 16), ([100, 64, 0, 0], 10)];
/// IPv6 loopback, unique local and link-local.
const PRIVATE_V6: [(u16, u8); 2] = [(0xfc00, 7), (0xfe80, 10)];

/// The CA as kept beside its key.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CaInfo {
    /// Its common name.
    name: String,
    /// The host name it may sign for, as it was when the CA was made.
    host_name: Option<String>,
    /// SHA-256 of its certificate, for a person to hold against what a PC shows.
    sha256: String,
}

/// The certificate as kept beside it: what it names and when it ends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LeafInfo {
    names: Vec<String>,
    not_before_unix_s: u64,
    not_after_unix_s: u64,
}

/// What a page shows of the certificate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct View {
    /// The CA's name and the SHA-256 of its certificate.
    pub ca_name: String,
    pub ca_sha256: String,
    /// What the certificate names.
    pub names: Vec<String>,
    /// Names and addresses the unit has that the CA may not sign for.
    pub left_out: Vec<String>,
    pub not_after_unix_s: u64,
}

struct State {
    ca: CaInfo,
    leaf: Option<LeafInfo>,
    left_out: Vec<String>,
}

pub struct Tls {
    dir: PathBuf,
    state: Mutex<State>,
}

fn io(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Write a key: readable by its owner alone.
fn write_secret(path: &Path, pem: &str) -> Result<(), String> {
    atomic_file::write(path, pem.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(())
}

fn private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => {
            let n = u32::from(a);
            PRIVATE_V4.iter().any(|(net, bits)| (n ^ u32::from_be_bytes(*net)) >> (32 - bits) == 0)
        }
        IpAddr::V6(a) => a.is_loopback() || PRIVATE_V6.iter().any(|(net, bits)| (a.segments()[0] ^ net) >> (16 - bits) == 0),
    }
}

/// Whether `name` is a host name as a certificate writes one: labels of letters, digits and
/// hyphens, 1 to 63 long, none starting or ending with a hyphen, 253 characters in all. A
/// computer's own name may not be one (an underscore in it).
fn dns_name(name: &str) -> bool {
    name.len() <= 253
        && name.split('.').all(|label| (1..=63).contains(&label.len()) && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') && !label.starts_with('-') && !label.ends_with('-'))
}

/// Whether the CA may sign for `name`: a private address, the unit's host name, or a local name.
fn permitted(name: &str, host_name: Option<&str>) -> bool {
    if let Ok(ip) = name.parse::<IpAddr>() {
        return private(ip);
    }
    dns_name(name) && (host_name == Some(name) || LOCAL_DOMAINS.iter().any(|d| name == *d || name.ends_with(&format!(".{d}"))))
}

fn ca_params(info: &CaInfo) -> Result<CertificateParams, String> {
    let mut p = CertificateParams::new(Vec::<String>::new()).map_err(io)?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::OrganizationName, "Fishball scanner");
    dn.push(DnType::CommonName, info.name.as_str());
    p.distinguished_name = dn;
    p.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let mut subtrees: Vec<GeneralSubtree> = LOCAL_DOMAINS.iter().map(|d| GeneralSubtree::DnsName(d.to_string())).collect();
    subtrees.extend(info.host_name.iter().map(|h| GeneralSubtree::DnsName(h.clone())));
    subtrees.extend(PRIVATE_V4.iter().map(|(net, bits)| GeneralSubtree::IpAddress(CidrSubnet::from_v4_prefix(*net, *bits))));
    subtrees.push(GeneralSubtree::IpAddress(CidrSubnet::from_v6_prefix(std::net::Ipv6Addr::LOCALHOST.octets(), 128)));
    for (net, bits) in PRIVATE_V6 {
        let mut addr = [0u8; 16];
        addr[..2].copy_from_slice(&net.to_be_bytes());
        subtrees.push(GeneralSubtree::IpAddress(CidrSubnet::from_v6_prefix(addr, bits)));
    }
    p.name_constraints = Some(NameConstraints { permitted_subtrees: subtrees, excluded_subtrees: Vec::new() });
    // The same on every board and at every start: the CA does not depend on the clock.
    p.not_before = date_time_ymd(2020, 1, 1);
    p.not_after = date_time_ymd(2100, 1, 1);
    Ok(p)
}

/// The start of the day of `unix_s`.
fn day_start(unix_s: u64) -> u64 {
    unix_s / DAY_S * DAY_S
}

impl Tls {
    fn file(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// Open the store in `dir`, making the CA when there is none. `host_name` is the unit's own.
    pub fn open(dir: &Path, host_name: Option<&str>) -> Result<Tls, String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let read = |name: &str| std::fs::read(dir.join(name)).ok();
        let kept: Option<CaInfo> = read("ca.json").and_then(|b| serde_json::from_slice(&b).ok());
        let ca = match kept.filter(|_| dir.join("ca.key").exists() && dir.join("ca.crt").exists()) {
            Some(info) => info,
            None => {
                let mut tag = [0u8; 4];
                aws_lc_rs::rand::fill(&mut tag).map_err(|_| "the system gave no random bytes".to_string())?;
                let mut info = CaInfo {
                    name: format!("Fishball scanner {} {}", host_name.unwrap_or("unit"), hex(&tag)),
                    host_name: host_name.filter(|h| dns_name(h)).map(str::to_string),
                    sha256: String::new(),
                };
                let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).map_err(io)?;
                let cert = ca_params(&info)?.self_signed(&key).map_err(io)?;
                info.sha256 = hex(digest::digest(&digest::SHA256, cert.der()).as_ref());
                write_secret(&dir.join("ca.key"), &key.serialize_pem())?;
                atomic_file::write(&dir.join("ca.crt"), cert.pem().as_bytes()).map_err(io)?;
                atomic_file::write_json(&dir.join("ca.json"), &info).map_err(io)?;
                // A certificate of an earlier CA is no use under this one.
                let _ = std::fs::remove_file(dir.join("server.json"));
                info
            }
        };
        let leaf = read("server.json").and_then(|b| serde_json::from_slice(&b).ok()).filter(|_: &LeafInfo| dir.join("server.crt").exists() && dir.join("server.key").exists());
        Ok(Tls { dir: dir.to_path_buf(), state: Mutex::new(State { ca, leaf, left_out: Vec::new() }) })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The CA as an issuer: its name and its key are what a signature needs.
    fn issuer(&self, info: &CaInfo) -> Result<(Certificate, KeyPair), String> {
        let pem = std::fs::read_to_string(self.file("ca.key")).map_err(|e| format!("the CA's key: {e}"))?;
        let key = KeyPair::from_pem(&pem).map_err(io)?;
        let cert = ca_params(info)?.self_signed(&key).map_err(io)?;
        Ok((cert, key))
    }

    /// See that the certificate names `names` and holds at `now_unix_s`, making it again when it
    /// does not. True when it was made. Names the CA may not sign for are left out.
    pub fn ensure(&self, names: &BTreeSet<String>, now_unix_s: u64) -> Result<bool, String> {
        let mut st = self.lock();
        let host = st.ca.host_name.clone();
        let (wanted, left_out): (Vec<String>, Vec<String>) = names.iter().cloned().partition(|n| permitted(n, host.as_deref()));
        st.left_out = left_out;
        let clock_set = now_unix_s >= CLOCK_FLOOR_S;
        let good = st.leaf.as_ref().is_some_and(|l| {
            l.names == wanted
                // Under a clock not set yet, the one there is is kept; made under one, it is made again.
                && (!clock_set || (l.not_before_unix_s >= CLOCK_FLOOR_S - DAY_S && now_unix_s + RENEW_DAYS * DAY_S < l.not_after_unix_s && now_unix_s + DAY_S >= l.not_before_unix_s))
        });
        if good {
            return Ok(false);
        }
        if wanted.is_empty() {
            return Err("the unit has no name its CA may sign for".into());
        }
        let (ca_cert, ca_key) = self.issuer(&st.ca)?;
        let not_before = day_start(now_unix_s.saturating_sub(DAY_S));
        let not_after = day_start(now_unix_s) + LASTS_DAYS * DAY_S;
        // A certificate's dates are whole days.
        let day = |unix_s: u64| {
            let (y, m, d, ..) = utc_fields(unix_s);
            date_time_ymd(y as i32, m as u8, d as u8)
        };
        let mut p = CertificateParams::new(wanted.clone()).map_err(io)?;
        let mut dn = DistinguishedName::new();
        dn.push(DnType::OrganizationName, "Fishball scanner");
        dn.push(DnType::CommonName, host.as_deref().unwrap_or("scanner"));
        p.distinguished_name = dn;
        p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        // Which key signed it: a strict verifier (Python's) refuses a certificate without it.
        p.use_authority_key_identifier_extension = true;
        p.not_before = day(not_before);
        p.not_after = day(not_after);
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).map_err(io)?;
        let cert = p.signed_by(&key, &ca_cert, &ca_key).map_err(io)?;
        let info = LeafInfo { names: wanted, not_before_unix_s: not_before, not_after_unix_s: not_after };
        // The record first gone, last written: a start between the files finds no certificate
        // and makes one, not a certificate and a key that do not belong together.
        let _ = std::fs::remove_file(self.file("server.json"));
        write_secret(&self.file("server.key"), &key.serialize_pem())?;
        atomic_file::write(&self.file("server.crt"), cert.pem().as_bytes()).map_err(io)?;
        atomic_file::write_json(&self.file("server.json"), &info).map_err(io)?;
        st.leaf = Some(info);
        Ok(true)
    }

    /// The certificate and its key, as PEM.
    pub fn pems(&self) -> Result<(Vec<u8>, Vec<u8>), String> {
        let read = |name: &str| std::fs::read(self.file(name)).map_err(|e| format!("{name}: {e}"));
        Ok((read("server.crt")?, read("server.key")?))
    }

    /// The CA's certificate, for a PC to install.
    pub fn ca_pem(&self) -> Result<Vec<u8>, String> {
        std::fs::read(self.file("ca.crt")).map_err(|e| format!("ca.crt: {e}"))
    }

    pub fn view(&self) -> View {
        let st = self.lock();
        View {
            ca_name: st.ca.name.clone(),
            ca_sha256: st.ca.sha256.clone(),
            names: st.leaf.as_ref().map(|l| l.names.clone()).unwrap_or_default(),
            left_out: st.left_out.clone(),
            not_after_unix_s: st.leaf.as_ref().map_or(0, |l| l.not_after_unix_s),
        }
    }
}

/// The addresses of the unit's interfaces.
pub fn addresses() -> Vec<IpAddr> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(list) = nix::ifaddrs::getifaddrs() {
            return list
                .filter_map(|i| {
                    let a = i.address?;
                    a.as_sockaddr_in().map(|v4| IpAddr::V4(v4.ip())).or_else(|| a.as_sockaddr_in6().map(|v6| IpAddr::V6(v6.ip())))
                })
                .collect();
        }
    }
    Vec::new()
}

/// The names the unit is reached by: `localhost` and the loopback address, its interfaces'
/// addresses, its host name bare and under `.local`, and the names its administrator added.
pub fn names(addresses: &[IpAddr], host_name: Option<&str>, added: &[String]) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = ["localhost".to_string(), "127.0.0.1".to_string()].into();
    out.extend(addresses.iter().map(|a| a.to_string()));
    if let Some(h) = host_name {
        out.insert(h.to_string());
        out.insert(format!("{h}.local"));
    }
    out.extend(added.iter().cloned());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-11-11 13:29:53 UTC.
    const NOW: u64 = 1_794_403_793;

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_ca_may_sign_for_private_addresses_and_local_names_only() {
        for name in ["192.168.120.50", "10.25.0.1", "172.16.5.4", "172.31.255.1", "127.0.0.1", "169.254.1.2", "100.64.3.2", "::1", "fd00::1", "fe80::1", "pluto", "pluto.local", "radio.lan", "x.home.arpa", "localhost"] {
            assert!(permitted(name, Some("pluto")), "{name}");
        }
        for name in ["8.8.8.8", "172.32.0.1", "100.128.0.1", "192.169.0.1", "2001:db8::1", "example.com", "pluto.example.com", "notlocal", "mylan"] {
            assert!(!permitted(name, Some("pluto")), "{name}");
        }
        assert!(!permitted("pluto", None), "a bare name is the unit's own or nothing");
        // Not host names as a certificate writes them, the unit's own or local as they may be.
        for name in ["nucbox_k6", "nucbox_k6.local", "radio..lan", ".lan", "-radio.lan", "radio-.lan", "", "ra dio.lan"] {
            assert!(!permitted(name, Some("nucbox_k6")), "{name:?}");
        }
        assert!(permitted(&format!("{}.lan", "a".repeat(63)), None) && !permitted(&format!("{}.lan", "a".repeat(64)), None));
    }

    #[test]
    fn a_host_name_no_certificate_can_carry_is_left_out_of_it_and_of_the_ca() {
        let dir = tempfile::tempdir().unwrap();
        let tls = Tls::open(dir.path(), Some("nucbox_k6")).unwrap();
        assert!(tls.ensure(&names(&[], Some("nucbox_k6"), &[]), NOW).unwrap());
        let v = tls.view();
        assert_eq!(v.names, ["127.0.0.1", "localhost"]);
        assert_eq!(v.left_out, ["nucbox_k6", "nucbox_k6.local"]);
        assert!(v.ca_name.starts_with("Fishball scanner nucbox_k6 "), "the CA's own name is for a person to read");
        assert_eq!(tls.lock().ca.host_name, None);
    }

    #[test]
    fn the_units_names_are_its_addresses_its_host_name_and_those_added() {
        let addrs = ["192.168.120.50".parse().unwrap(), "192.168.2.1".parse().unwrap()];
        assert_eq!(
            names(&addrs, Some("pluto"), &["radio.lan".into()]).into_iter().collect::<Vec<_>>(),
            ["127.0.0.1", "192.168.120.50", "192.168.2.1", "localhost", "pluto", "pluto.local", "radio.lan"]
        );
        assert_eq!(names(&[], None, &[]).into_iter().collect::<Vec<_>>(), ["127.0.0.1", "localhost"]);
    }

    #[test]
    fn a_certificate_is_made_once_and_again_when_its_names_change() {
        let dir = tempfile::tempdir().unwrap();
        let tls = Tls::open(dir.path(), Some("pluto")).unwrap();
        let first = set(&["127.0.0.1", "192.168.120.50", "localhost", "pluto", "pluto.local", "8.8.8.8", "radio.example.com"]);
        assert!(tls.ensure(&first, NOW).unwrap(), "none yet: made");
        let v = tls.view();
        assert_eq!(v.names, ["127.0.0.1", "192.168.120.50", "localhost", "pluto", "pluto.local"]);
        assert_eq!(v.left_out, ["8.8.8.8", "radio.example.com"], "what its CA may not sign for");
        assert!(v.ca_name.starts_with("Fishball scanner pluto ") && v.ca_sha256.len() == 64);
        assert_eq!(v.not_after_unix_s, day_start(NOW) + LASTS_DAYS * DAY_S);
        let (cert, key) = tls.pems().unwrap();
        assert!(cert.starts_with(b"-----BEGIN CERTIFICATE-----") && key.starts_with(b"-----BEGIN PRIVATE KEY-----"));
        assert!(tls.ca_pem().unwrap().starts_with(b"-----BEGIN CERTIFICATE-----"));
        // The same names a minute later, and a month before its end: kept.
        assert!(!tls.ensure(&first, NOW + 60).unwrap());
        assert!(!tls.ensure(&first, v.not_after_unix_s - (RENEW_DAYS + 1) * DAY_S).unwrap());
        // The address changed (a new lease): made again, under the same CA.
        let moved = set(&["127.0.0.1", "192.168.120.77", "localhost", "pluto", "pluto.local"]);
        assert!(tls.ensure(&moved, NOW + 120).unwrap());
        assert_eq!(tls.view().names, ["127.0.0.1", "192.168.120.77", "localhost", "pluto", "pluto.local"]);
        assert_eq!(tls.view().ca_sha256, v.ca_sha256);
        assert_ne!(tls.pems().unwrap().0, cert);
        // Near its end: made again.
        assert!(tls.ensure(&moved, tls.view().not_after_unix_s - 5 * DAY_S).unwrap());
    }

    #[test]
    fn the_ca_and_the_certificate_are_kept_across_a_start() {
        let dir = tempfile::tempdir().unwrap();
        let names = set(&["127.0.0.1", "localhost"]);
        let (ca, cert) = {
            let tls = Tls::open(dir.path(), Some("pluto")).unwrap();
            tls.ensure(&names, NOW).unwrap();
            (tls.ca_pem().unwrap(), tls.pems().unwrap().0)
        };
        let again = Tls::open(dir.path(), Some("pluto")).unwrap();
        assert_eq!(again.ca_pem().unwrap(), ca, "the CA is made once");
        assert!(!again.ensure(&names, NOW + 3600).unwrap(), "its certificate still holds");
        assert_eq!(again.pems().unwrap().0, cert);
        // The key gone (a flash half written): a new CA, and a certificate under it.
        std::fs::remove_file(dir.path().join("ca.key")).unwrap();
        let remade = Tls::open(dir.path(), Some("pluto")).unwrap();
        assert_ne!(remade.ca_pem().unwrap(), ca);
        assert!(remade.ensure(&names, NOW + 7200).unwrap());
    }

    #[test]
    fn a_certificate_made_before_the_clock_was_set_is_made_again_after() {
        let dir = tempfile::tempdir().unwrap();
        let tls = Tls::open(dir.path(), None).unwrap();
        let names = set(&["127.0.0.1", "localhost"]);
        // The board starts in 1970.
        assert!(tls.ensure(&names, 40).unwrap());
        assert!(!tls.ensure(&names, 400).unwrap(), "nothing to gain while the clock is not set");
        // The site sets the clock.
        assert!(tls.ensure(&names, NOW).unwrap());
        assert!(!tls.ensure(&names, NOW + 60).unwrap());
        // The clock lost again at a restart: the good certificate is kept.
        assert!(!tls.ensure(&names, 40).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn the_keys_are_their_owners_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let tls = Tls::open(dir.path(), None).unwrap();
        tls.ensure(&set(&["127.0.0.1"]), NOW).unwrap();
        for key in ["ca.key", "server.key"] {
            assert_eq!(std::fs::metadata(dir.path().join(key)).unwrap().permissions().mode() & 0o777, 0o600, "{key}");
        }
    }
}
