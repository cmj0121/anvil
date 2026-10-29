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
