//! Best-effort check that this computer's firewall lets the printer reach the WSD
//! listener. When it doesn't, the printer panel shows a communication error and
//! nothing at all arrives here, so there is nothing to notice at scan time. All we
//! can do is read the firewall's configuration up front.
//!
//! Only ufw is understood; other firewalls are not detected.

use std::fs;
use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::path::Path;

use log::{debug, error, info};

#[derive(Debug, PartialEq)]
enum Verdict {
    Allowed,
    Blocked,
    /// A rule that may apply uses something this check doesn't understand.
    Unclear,
}

/// Log an error if ufw will drop the printer's connections to `port`.
pub fn check(printer_host: &str, port: u16) {
    let Some((printer, local)) = addresses(printer_host) else {
        return;
    };
    let Some(verdict) = ufw(Path::new("/etc"), printer, local, port) else {
        return;
    };
    let fix = format!("sudo ufw allow from {printer} to any port {port} proto tcp");
    match verdict {
        Verdict::Allowed => debug!("ufw lets {printer} reach port {port}"),
        Verdict::Blocked => error!(
            "this computer's firewall (ufw) blocks the printer from reaching port {port}: scans \
             started on the printer panel will fail there with a communication error. To allow it: {fix}"
        ),
        Verdict::Unclear => info!(
            "this computer's firewall (ufw) is on and its rules for port {port} couldn't be checked. \
             If the printer panel shows a communication error: {fix}"
        ),
    }
}

/// What ufw, configured under `etc`, does with the printer's connections to `port`.
/// `None` when ufw is off or lets everything in.
fn ufw(etc: &Path, printer: Ipv4Addr, local: Ipv4Addr, port: u16) -> Option<Verdict> {
    let conf = fs::read_to_string(etc.join("ufw/ufw.conf")).unwrap_or_default();
    let defaults = fs::read_to_string(etc.join("default/ufw")).unwrap_or_default();
    if !setting(&conf, "ENABLED").is_some_and(|v| v.eq_ignore_ascii_case("yes"))
        || setting(&defaults, "DEFAULT_INPUT_POLICY").is_some_and(|v| v.eq_ignore_ascii_case("ACCEPT"))
    {
        return None;
    }
    // The rules are only readable by root on some systems.
    Some(
        fs::read_to_string(etc.join("ufw/user.rules"))
            .map_or(Verdict::Unclear, |rules| ufw_verdict(&rules, printer, local, port)),
    )
}

/// The printer's address and ours on the route to it (IPv4 only).
fn addresses(printer_host: &str) -> Option<(Ipv4Addr, Ipv4Addr)> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect((printer_host, 3702)).ok()?;
    match (s.peer_addr().ok()?.ip(), s.local_addr().ok()?.ip()) {
        (IpAddr::V4(printer), IpAddr::V4(local)) => Some((printer, local)),
        _ => None,
    }
}

/// `KEY=value` or `KEY="value"` from a shell-style settings file.
fn setting<'a>(file: &'a str, key: &str) -> Option<&'a str> {
    file.lines().find_map(|l| {
        let value = l.trim().strip_prefix(key)?.trim_start().strip_prefix('=')?;
        Some(value.trim().trim_matches('"'))
    })
}

/// What ufw's user rules (`/etc/ufw/user.rules`, iptables syntax) do with a TCP
/// connection from `src` to `dst`:`port`, given that anything unmatched is dropped.
fn ufw_verdict(rules: &str, src: Ipv4Addr, dst: Ipv4Addr, port: u16) -> Verdict {
    for line in rules.lines() {
        let Some(rule) = line.trim().strip_prefix("-A ufw-user-input ") else {
            continue;
        };
        let mut applies = true;
        let mut understood = true;
        let mut target = None;
        let mut tokens = rule.split_whitespace();
        while let Some(t) = tokens.next() {
            match t {
                "-p" => applies &= matches!(tokens.next(), Some("tcp" | "all")),
                "--dport" | "--dports" => match tokens.next().and_then(|v| ports_include(v, port)) {
                    Some(yes) => applies &= yes,
                    None => understood = false,
                },
                "-s" | "-d" => {
                    let ip = if t == "-s" { src } else { dst };
                    match tokens.next().and_then(|v| net_includes(v, ip)) {
                        Some(yes) => applies &= yes,
                        None => understood = false,
                    }
                }
                "-j" => target = tokens.next(),
                "-m" => {
                    if tokens.next() != Some("multiport") {
                        understood = false;
                    }
                }
                // Anything else (an interface, a source port, a negation, ...).
                _ => understood = false,
            }
        }
        if !applies {
            continue;
        }
        match target {
            // `ufw limit`: a counting rule and a rate check, then an accept rule.
            None | Some("ufw-user-limit") => continue,
            Some("ACCEPT" | "ufw-user-limit-accept") if understood => return Verdict::Allowed,
            Some("DROP" | "REJECT") if understood => return Verdict::Blocked,
            _ => return Verdict::Unclear,
        }
    }
    Verdict::Blocked
}

/// Whether an iptables port spec ("80", "5000:6000", "22,80,5000:6000") includes `port`.
fn ports_include(spec: &str, port: u16) -> Option<bool> {
    let mut found = false;
    for part in spec.split(',') {
        let (lo, hi) = part.split_once(':').unwrap_or((part, part));
        found |= (lo.parse::<u16>().ok()?..=hi.parse::<u16>().ok()?).contains(&port);
    }
    Some(found)
}

/// Whether an address or CIDR network ("192.168.1.0/24") includes `ip`.
fn net_includes(spec: &str, ip: Ipv4Addr) -> Option<bool> {
    let (addr, bits) = spec.split_once('/').unwrap_or((spec, "32"));
    let bits: u32 = bits.parse().ok().filter(|b| *b <= 32)?;
    let mask = u32::MAX.checked_shl(32 - bits).unwrap_or(0);
    Some(addr.parse::<Ipv4Addr>().ok()?.to_bits() & mask == ip.to_bits() & mask)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRINTER: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 50);
    const US: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 20);

    fn verdict(rules: &str) -> Verdict {
        ufw_verdict(rules, PRINTER, US, 5309)
    }

    /// A desktop's rules: a file-sharing port, Docker's DNS, and SSH from the LAN.
    const BASE: &str = "*filter\n:ufw-user-input - [0:0]\n\
        ### tuple ### allow tcp 53317 0.0.0.0/0 any 0.0.0.0/0 in\n\
        -A ufw-user-input -p tcp --dport 53317 -j ACCEPT\n\
        -A ufw-user-input -p udp -d 172.17.0.1 --dport 53 -s 192.168.0.0/16 -j ACCEPT\n\
        -A ufw-user-input -p tcp --dport 22 -s 192.168.1.0/24 -j ACCEPT\n";

    #[test]
    fn unlisted_port_is_blocked() {
        assert_eq!(verdict(BASE), Verdict::Blocked);
        assert_eq!(verdict(""), Verdict::Blocked);
    }

    #[test]
    fn allowed_for_the_printer() {
        let rule = "-A ufw-user-input -p tcp --dport 5309 -s 192.168.1.50 -j ACCEPT\n";
        assert_eq!(verdict(&format!("{BASE}{rule}")), Verdict::Allowed);
    }

    #[test]
    fn allowed_by_wider_rules() {
        for rule in [
            "-A ufw-user-input -p tcp --dport 5309 -j ACCEPT",
            "-A ufw-user-input -s 192.168.1.0/24 -j ACCEPT",
            "-A ufw-user-input -p tcp -m multiport --dports 80,5000:6000 -j ACCEPT",
            "-A ufw-user-input -p tcp -d 192.168.1.20 --dport 5309 -s 192.168.1.50 -j ACCEPT",
            "-A ufw-user-input -p tcp --dport 5309 -j ufw-user-limit-accept",
        ] {
            assert_eq!(verdict(rule), Verdict::Allowed, "{rule}");
        }
    }

    #[test]
    fn rules_for_something_else_dont_count() {
        for rule in [
            "-A ufw-user-input -p udp --dport 5309 -j ACCEPT",
            "-A ufw-user-input -p tcp --dport 5309 -s 192.168.1.51 -j ACCEPT",
            "-A ufw-user-input -p tcp --dport 5309 -s 10.0.0.0/8 -j ACCEPT",
            "-A ufw-user-input -p tcp -m multiport --dports 80,443 -j ACCEPT",
            "-A ufw-user-input -p tcp -d 192.168.1.99 --dport 5309 -j ACCEPT",
        ] {
            assert_eq!(verdict(rule), Verdict::Blocked, "{rule}");
        }
    }

    #[test]
    fn first_matching_rule_wins() {
        let deny = "-A ufw-user-input -p tcp --dport 5309 -j DROP\n";
        let allow = "-A ufw-user-input -p tcp --dport 5309 -j ACCEPT\n";
        assert_eq!(verdict(&format!("{deny}{allow}")), Verdict::Blocked);
        assert_eq!(verdict(&format!("{allow}{deny}")), Verdict::Allowed);
    }

    #[test]
    fn rules_it_cant_read_are_unclear() {
        for rule in [
            "-A ufw-user-input -i eth0 -p tcp --dport 5309 -j ACCEPT",
            "-A ufw-user-input -p tcp --dport 5309 ! -s 192.168.1.50 -j ACCEPT",
            "-A ufw-user-input -p tcp --dport 5309 -s printer.lan -j ACCEPT",
        ] {
            assert_eq!(verdict(rule), Verdict::Unclear, "{rule}");
        }
    }

    #[test]
    fn rate_limited_port_is_allowed() {
        let rules = "-A ufw-user-input -p tcp --dport 5309 -m conntrack --ctstate NEW -m recent --set\n\
            -A ufw-user-input -p tcp --dport 5309 -m conntrack --ctstate NEW -m recent --update --seconds 30 --hitcount 6 -j ufw-user-limit\n\
            -A ufw-user-input -p tcp --dport 5309 -j ufw-user-limit-accept\n";
        assert_eq!(verdict(rules), Verdict::Allowed);
    }

    /// ufw's files as it writes them, in a scratch `etc`.
    #[test]
    fn reads_ufw_files() {
        let etc = std::env::temp_dir().join(format!("scanai-ufw-test-{}", std::process::id()));
        fs::create_dir_all(etc.join("ufw")).unwrap();
        fs::create_dir_all(etc.join("default")).unwrap();
        let write = |file: &str, text: &str| fs::write(etc.join(file), text).unwrap();
        let state = || ufw(&etc, PRINTER, US, 5309);
        let conf = |enabled: &str| {
            format!("# /etc/ufw/ufw.conf\n#\n\n# Set to yes to start on boot.\nENABLED={enabled}\n\nLOGLEVEL=low\n")
        };
        let rules = |extra: &str| {
            format!(
                "*filter\n:ufw-user-input - [0:0]\n:ufw-user-limit - [0:0]\n### RULES ###\n\n\
                 ### tuple ### allow tcp 22 0.0.0.0/0 any 192.168.1.0/24 in\n\
                 -A ufw-user-input -p tcp --dport 22 -s 192.168.1.0/24 -j ACCEPT\n\n{extra}\
                 ### END RULES ###\n\n### LOGGING ###\n\
                 -A ufw-after-logging-input -j LOG --log-prefix \"[UFW BLOCK] \" -m limit --limit 3/min --limit-burst 10\n\
                 ### END LOGGING ###\n\n### RATE LIMITING ###\n\
                 -A ufw-user-limit -j REJECT\n-A ufw-user-limit-accept -j ACCEPT\n### END RATE LIMITING ###\nCOMMIT\n"
            )
        };

        // Not installed, or switched off.
        assert_eq!(state(), None);
        write("ufw/ufw.conf", &conf("no"));
        write("default/ufw", "# Set the default input policy to ACCEPT, DROP, or REJECT.\nDEFAULT_INPUT_POLICY=\"DROP\"\n");
        write("ufw/user.rules", &rules(""));
        assert_eq!(state(), None);

        write("ufw/ufw.conf", &conf("yes"));
        assert_eq!(state(), Some(Verdict::Blocked));
        write(
            "ufw/user.rules",
            &rules("### tuple ### allow tcp 5309 0.0.0.0/0 any 192.168.1.50 in\n-A ufw-user-input -p tcp --dport 5309 -s 192.168.1.50 -j ACCEPT\n\n"),
        );
        assert_eq!(state(), Some(Verdict::Allowed));

        // Rules only root may read.
        fs::remove_file(etc.join("ufw/user.rules")).unwrap();
        assert_eq!(state(), Some(Verdict::Unclear));

        // Everything is let in by default.
        write("default/ufw", "DEFAULT_INPUT_POLICY=\"ACCEPT\"\n");
        assert_eq!(state(), None);
        fs::remove_dir_all(&etc).unwrap();
    }

    #[test]
    fn settings() {
        let conf = "# comment\nENABLED=yes\nLOGLEVEL=low\n";
        assert_eq!(setting(conf, "ENABLED"), Some("yes"));
        assert_eq!(setting("DEFAULT_INPUT_POLICY=\"DROP\"\n", "DEFAULT_INPUT_POLICY"), Some("DROP"));
        assert_eq!(setting(conf, "MISSING"), None);
    }
}
