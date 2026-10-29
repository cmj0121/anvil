//! The hardening rule catalogue.

/// One hardening rule; `strict` rules run only under `--strict`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rule {
    pub id: &'static str,
    pub strict: bool,
    pub summary: &'static str,
}

/// Every rule, in the order suggestions are made.
pub const RULES: &[Rule] = &[
    Rule {
        id: "ssh.root-login",
        strict: false,
        summary: "refuse ssh logins as root",
    },
    Rule {
        id: "ssh.password-auth",
        strict: true,
        summary: "refuse ssh password and keyboard-interactive logins",
    },
    Rule {
        id: "sysctl.net",
        strict: false,
        summary: "harden kernel network settings",
    },
    Rule {
        id: "updates.auto",
        strict: false,
        summary: "install security updates automatically",
    },
    Rule {
        id: "packages.insecure",
        strict: false,
        summary: "remove insecure legacy network packages",
    },
];

/// The rule named `id`.
pub fn find(id: &str) -> Option<&'static Rule> {
    RULES.iter().find(|rule| rule.id == id)
}

/// The first line of every generated store file.
pub const MANAGED: &str = "# managed by anvil config --harden\n";

/// The store file and dest of the sshd drop-in.
pub const SSHD_STORE: &str = "$/harden/sshd.conf";
pub const SSHD_DEST: &str = "/etc/ssh/sshd_config.d/00-anvil-harden.conf";

/// The sshd directives each ssh rule requires.
pub const ROOT_LOGIN: &[(&str, &str)] = &[("PermitRootLogin", "no")];
pub const PASSWORD_AUTH: &[(&str, &str)] = &[
    ("PasswordAuthentication", "no"),
    ("KbdInteractiveAuthentication", "no"),
];

/// The store file and dest of the sysctl drop-in.
pub const SYSCTL_STORE: &str = "$/harden/sysctl.conf";
pub const SYSCTL_DEST: &str = "/etc/sysctl.d/99-anvil-harden.conf";

/// The kernel settings `sysctl.net` requires.
pub const SYSCTL_NET: &[(&str, &str)] = &[
    ("net.ipv4.conf.all.rp_filter", "2"),
    ("net.ipv4.conf.default.rp_filter", "2"),
    ("net.ipv4.conf.all.accept_redirects", "0"),
    ("net.ipv4.conf.default.accept_redirects", "0"),
    ("net.ipv6.conf.all.accept_redirects", "0"),
    ("net.ipv6.conf.default.accept_redirects", "0"),
    ("net.ipv4.conf.all.send_redirects", "0"),
    ("net.ipv4.conf.default.send_redirects", "0"),
    ("net.ipv4.conf.all.accept_source_route", "0"),
    ("net.ipv4.conf.default.accept_source_route", "0"),
    ("net.ipv4.icmp_echo_ignore_broadcasts", "1"),
    ("net.ipv4.tcp_syncookies", "1"),
    ("net.ipv4.conf.all.log_martians", "1"),
];

/// Packages `packages.insecure` moves from `install` to `remove`.
pub const INSECURE: &[&str] = &[
    "ftp",
    "inetutils-telnet",
    "inetutils-telnetd",
    "nis",
    "rsh",
    "rsh-client",
    "rsh-redone-client",
    "rsh-server",
    "talk",
    "talkd",
    "telnet",
    "telnet-server",
    "telnetd",
    "tftp",
    "tftp-hpa",
    "tftp-server",
    "tftpd",
    "tftpd-hpa",
    "ypbind",
];

/// The marker line that names a generated step.
pub fn marker(name: &str) -> String {
    format!("# anvil-harden: {name}")
}

/// The Linux guard and root helper every generated step starts with, after
/// its marker.
macro_rules! head {
    () => {
        concat!(
            "[ \"$(uname -s)\" = Linux ] || exit 0\n",
            "PATH=\"$PATH:/usr/sbin:/sbin\"\n",
            "if [ \"$(id -u)\" -eq 0 ]; then S=; else S=\"sudo -n\"; fi\n",
        )
    };
}

/// Checks, reloads and verifies the sshd drop-in; it removes the drop-in
/// when sshd would reject it.
pub const SSH_STEP: &str = concat!(
    "# anvil-harden: ssh\n",
    head!(),
    r#"f=/etc/ssh/sshd_config.d/00-anvil-harden.conf
[ -f "$f" ] || exit 0
command -v sshd >/dev/null 2>&1 || { echo "anvil-harden: sshd not installed; skipped" >&2; exit 0; }
if ! $S grep -Eqi '^[[:space:]]*Include[[:space:]]+(/etc/ssh/)?sshd_config\.d/' /etc/ssh/sshd_config; then
  $S rm -f "$f"; echo "anvil-harden: /etc/ssh/sshd_config has no Include for sshd_config.d; removed $f" >&2; exit 1
fi
if ! $S sshd -t; then $S rm -f "$f"; echo "anvil-harden: sshd -t failed; removed $f" >&2; exit 1; fi
if command -v systemctl >/dev/null 2>&1; then
  $S systemctl reload ssh 2>/dev/null || $S systemctl reload sshd || exit 1
elif command -v rc-service >/dev/null 2>&1; then $S rc-service sshd reload || exit 1
else echo "anvil-harden: no systemctl or rc-service to reload sshd" >&2; exit 1; fi
t=$($S sshd -T) || exit 1
grep -Ev '^[[:space:]]*(#|$)' "$f" | while read -r k v; do
  printf '%s\n' "$t" | grep -qix "$(printf '%s %s' "$k" "$v")" || { echo "anvil-harden: sshd -T does not show $k $v" >&2; exit 1; }
done || exit 1
"#
);

/// Loads the sysctl drop-in.
pub const SYSCTL_STEP: &str = concat!(
    "# anvil-harden: sysctl\n",
    head!(),
    r#"f=/etc/sysctl.d/99-anvil-harden.conf; [ -f "$f" ] || exit 0; $S sysctl -e -p "$f" >/dev/null
"#
);

/// Installs and enables automatic security updates.
pub const UPDATES_STEP: &str = concat!(
    "# anvil-harden: updates\n",
    head!(),
    r#"if command -v apt-get >/dev/null 2>&1; then
  $S env DEBIAN_FRONTEND=noninteractive apt-get install -y unattended-upgrades >/dev/null || exit 1
  printf 'APT::Periodic::Update-Package-Lists "1";\nAPT::Periodic::Unattended-Upgrade "1";\n' |
    $S tee /etc/apt/apt.conf.d/20auto-upgrades >/dev/null
elif command -v dnf >/dev/null 2>&1; then
  $S dnf install -y dnf-automatic >/dev/null || exit 1
  c=/etc/dnf/automatic.conf
  if [ -f "$c" ]; then $S sed -i 's/^upgrade_type.*/upgrade_type = security/;s/^apply_updates.*/apply_updates = yes/' "$c"
  else printf '[commands]\nupgrade_type = security\napply_updates = yes\n' | $S tee "$c" >/dev/null; fi
  $S systemctl enable --now dnf-automatic.timer >/dev/null 2>&1 || $S systemctl enable --now dnf5-automatic.timer
else
  echo "anvil-harden: no automatic security updates for this package manager; skipped" >&2
fi
"#
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_in_order_and_only_password_auth_is_strict() {
        let ids: Vec<_> = RULES.iter().map(|rule| rule.id).collect();
        assert_eq!(
            ids,
            [
                "ssh.root-login",
                "ssh.password-auth",
                "sysctl.net",
                "updates.auto",
                "packages.insecure"
            ]
        );
        let strict: Vec<_> = RULES.iter().filter(|r| r.strict).map(|r| r.id).collect();
        assert_eq!(strict, ["ssh.password-auth"]);
        assert_eq!(find("sysctl.net").map(|r| r.id), Some("sysctl.net"));
        assert_eq!(find("nope"), None);
    }
}
