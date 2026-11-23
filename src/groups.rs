//! `[[groups]]`: a local group created, its gid fixed, or removed, on each
//! target whose tags and facts all match.
//!
//! [`run`] turns each entry into one step per target: a skip when the entry
//! does not match, else one script, run as root or through sudo, with the
//! password typed when sudo asks for one. The tools come from the probe's
//! `accounts` and `accounts_bin` facts and are called by absolute path, as a
//! non-login ssh `PATH` may lack sbin; a target with neither shadow-utils nor
//! busybox fails the step.
//!
//! [`script`] reads the group with `getent group`, or from `/etc/group` with
//! awk when getent is missing, and changes only what differs: it prints
//! `created group NAME`, `fixed group NAME: gid`, `removed group NAME` or
//! `unchanged group NAME`. A gid another group holds, a gid change on
//! busybox, and removing some user's primary group fail the step before any
//! change. A group with no `gid` is never renumbered. Nothing is recorded
//! for rollback.

use crate::config::{Config, GroupEntry, is_account_name};
use crate::facts::Facts;
use crate::files::sudo_secret_for;
use crate::inventory::Target;
use crate::remote::quote;
use crate::step::{GroupRun, PlannedRun, RunKind};
use crate::tasks::Item;
use crate::transport::RUN_TIMEOUT;

/// The failure of a step on a target with no account tools.
pub const NO_TOOLS: &str = "groups: no account tools on this target (accounts = none)";

/// The account tools a target has, from its `accounts` fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accounts {
    /// shadow-utils: `groupadd`, `groupmod`, `groupdel`.
    Shadow,
    /// busybox: `addgroup`, `delgroup`; no gid change.
    Busybox,
}

impl Accounts {
    /// The tools and the absolute directory that holds them; `None` for
    /// `accounts = none`, a missing fact, or a directory that is not absolute.
    pub fn of(facts: &Facts) -> Option<(Accounts, &str)> {
        let accounts = match facts.get("accounts")? {
            "shadow" => Accounts::Shadow,
            "busybox" => Accounts::Busybox,
            _ => return None,
        };
        let bin = facts
            .get("accounts_bin")
            .filter(|bin| bin.starts_with('/'))?;
        Some((accounts, bin))
    }
}

/// The step of group `item`, holding `entry`, the `i`th group item.
pub fn run(
    config: &Config,
    item: &Item,
    (entry, i): (&GroupEntry, usize),
    target: &Target,
    facts: Option<&Facts>,
) -> Result<PlannedRun, String> {
    let facts = facts.ok_or("groups: the probe has not run")?;
    let label = item.label("groups", i);
    let name = &entry.name;
    if !item.matches(target, facts) {
        log::info!("{}: {label} {name}: no match", target.name);
        return Ok(group_run(
            name,
            String::new(),
            format!("skipped group {name}: no match"),
            None,
            true,
        ));
    }
    let Some((accounts, bin)) = Accounts::of(facts) else {
        log::info!("{}: {label} {name}: no account tools", target.name);
        return Ok(group_run(
            name,
            format!("{{ {}\n}}\n", fail(NO_TOOLS)),
            shown(entry),
            None,
            false,
        ));
    };
    let root = facts.is_root();
    let secret =
        sudo_secret_for(config, target).filter(|_| entry.sudo && facts.sudo_asks_password());
    let sudo = match secret {
        Some(_) => "sudo ",
        None if root || !entry.sudo => "",
        None => "sudo -n ",
    };
    log::info!("{}: {label} {name} with {accounts:?} in {bin}", target.name);
    Ok(group_run(
        name,
        script(&label, entry, (accounts, bin), sudo),
        shown(entry),
        secret.map(str::to_owned),
        false,
    ))
}

/// What the step shows, from the config alone: `group NAME`, with `(gid N,
/// system)` for those set, or `remove group NAME`.
pub fn shown(entry: &GroupEntry) -> String {
    let name = &entry.name;
    if entry.absent() {
        return format!("remove group {name}");
    }
    let attrs: Vec<String> = [
        entry.gid.map(|gid| format!("gid {gid}")),
        entry.system.then(|| "system".to_owned()),
    ]
    .into_iter()
    .flatten()
    .collect();
    match attrs.as_slice() {
        [] => format!("group {name}"),
        attrs => format!("group {name} ({})", attrs.join(", ")),
    }
}

fn group_run(
    name: &str,
    run: String,
    label: String,
    sudo_secret: Option<String>,
    skip: bool,
) -> PlannedRun {
    PlannedRun {
        run: run.into(),
        label: Some(label),
        sudo_secret,
        timeout: RUN_TIMEOUT,
        kind: RunKind::Group {
            name: name.to_owned(),
            run: GroupRun { skip },
        },
        task: None,
        name: None,
        notify: Vec::new(),
    }
}

/// `message` on stderr, then exit 1.
fn fail(message: &str) -> String {
    format!("{{ echo {} >&2; exit 1; }}", quote(message))
}

/// One sh brace group that brings group `entry` to its state with the
/// `accounts` tools in `bin`; `label` names the entry in its failures and
/// `sudo` is "" (root), "sudo -n " or "sudo " (a password typed). Reads need
/// no root; with `sudo` set, `sudo true` runs first, so a sudo failure is
/// reported once, as sudo's message. The name is inserted bare: its charset
/// needs no quoting and it never starts with `-`. The tools' own output goes
/// to stderr, so stdout holds only the script's line.
pub fn script(
    label: &str,
    entry: &GroupEntry,
    (accounts, bin): (Accounts, &str),
    sudo: &str,
) -> String {
    let name = entry.name.as_str();
    debug_assert!(is_account_name(name), "{name}");
    let bin = quote(bin);
    let check = if sudo.is_empty() {
        String::new()
    } else {
        format!("{sudo}true || exit\n")
    };
    let line = |text: String| format!("echo {}\n", quote(&text));
    let unchanged = line(format!("unchanged group {name}"));
    let body = if entry.absent() {
        let delete = match accounts {
            Accounts::Shadow => format!("{sudo}{bin}/groupdel {name}"),
            Accounts::Busybox => format!("{sudo}{bin}/delgroup {name}"),
        };
        format!(
            "if [ -z \"$anvil_line\" ]; then {unchanged}else\n\
             anvil_user=$({{ if [ -n \"$anvil_g\" ]; then getent passwd; else cat /etc/passwd; fi; }} \
             | awk -F: -v g=\"$anvil_gid\" '$4 == g {{ print $1; exit }}')\n\
             [ -z \"$anvil_user\" ] || {{ echo {primary}\"$anvil_user\" >&2; exit 1; }}\n\
             {delete} >&2 || exit\n\
             {removed}fi\n",
            primary = quote(&format!(
                "{label} {name}: cannot remove the primary group of user "
            )),
            removed = line(format!("removed group {name}")),
        )
    } else {
        let gid = entry.gid;
        let flags = [
            gid.map(|gid| format!(" -g {gid}")),
            entry.system.then(|| match accounts {
                Accounts::Shadow => " -r".to_owned(),
                Accounts::Busybox => " -S".to_owned(),
            }),
        ]
        .into_iter()
        .flatten()
        .collect::<String>();
        let held = gid
            .map(|gid| {
                format!(
                    "anvil_held=$(anvil_get group {gid} 3) && \
                     {{ echo {held}\"${{anvil_held%%:*}}\" >&2; exit 1; }}\n",
                    held = quote(&format!("{label} {name}: gid {gid} is held by group ")),
                )
            })
            .unwrap_or_default();
        let create = match accounts {
            Accounts::Shadow => format!("{sudo}{bin}/groupadd{flags} {name}"),
            Accounts::Busybox => format!("{sudo}{bin}/addgroup{flags} {name}"),
        };
        let created = line(format!("created group {name}"));
        let fix = match (gid, accounts) {
            (None, _) => String::new(),
            (Some(gid), Accounts::Shadow) => format!(
                "elif [ \"$anvil_gid\" != {gid} ]; then\n{held}\
                 {sudo}{bin}/groupmod -g {gid} {name} >&2 || exit\n{}",
                line(format!("fixed group {name}: gid"))
            ),
            (Some(gid), Accounts::Busybox) => format!(
                "elif [ \"$anvil_gid\" != {gid} ]; then {}\n",
                fail(&format!(
                    "{label} {name}: busybox cannot change gid; install shadow"
                ))
            ),
        };
        format!(
            "if [ -z \"$anvil_line\" ]; then\n{held}{create} >&2 || exit\n{created}\
             {fix}else {unchanged}fi\n"
        )
    };
    format!(
        "{{ LC_ALL=C; export LC_ALL; set -f\n\
         {check}\
         anvil_g=; command -v getent >/dev/null 2>&1 && anvil_g=1\n\
         anvil_get() {{ if [ -n \"$anvil_g\" ]; then getent \"$1\" \"$2\"; else \
         awk -F: -v k=\"$2\" -v f=\"$3\" '$f == k {{ print; n = 1; exit }} END {{ exit n ? 0 : 2 }}' \
         \"/etc/$1\"; fi; }}\n\
         anvil_line=$(anvil_get group {name} 1); anvil_rc=$?\n\
         case $anvil_rc in 0) ;; 2) anvil_line= ;; *) {unread} ;; esac\n\
         anvil_gid=${{anvil_line#*:*:}}; anvil_gid=${{anvil_gid%%:*}}\n\
         {body}}}\n",
        unread = fail(&format!("groups: cannot read group {name}")),
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::test_util::{chmod, temp_dir};
    use crate::transport::{Local, Transport};

    fn entry(keys: &str) -> GroupEntry {
        toml::from_str(&format!("name = \"app\"\n{keys}")).unwrap()
    }

    fn facts(text: &str) -> Facts {
        Facts::parse(format!("{text}anvil-probe-end=1\n").as_bytes()).unwrap()
    }

    /// A stub `getent` over `group` and `passwd` files in `dir`, and stub
    /// account tools in `dir/bin` that log their arguments to `dir/log`.
    struct Fake {
        dir: PathBuf,
    }

    impl Fake {
        fn new(group: &str, passwd: &str) -> Fake {
            let dir = temp_dir("groups");
            fs::create_dir(dir.join("bin")).unwrap();
            fs::write(dir.join("group"), group).unwrap();
            fs::write(dir.join("passwd"), passwd).unwrap();
            let getent = format!(
                "#!/bin/sh\n[ $# -eq 1 ] && exec cat '{0}'/\"$1\"\n\
                 awk -F: -v k=\"$2\" '$1 == k || $3 == k {{ print; f = 1 }} END {{ exit f ? 0 : 2 }}' \
                 '{0}'/\"$1\"\n",
                dir.display()
            );
            Self::tool(&dir.join("getent"), &getent);
            for tool in ["groupadd", "groupmod", "groupdel", "addgroup", "delgroup"] {
                let log = format!(
                    "#!/bin/sh\necho \"{tool} $*\" >>'{}'/log\necho {tool} ran\n",
                    dir.display()
                );
                Self::tool(&dir.join("bin").join(tool), &log);
            }
            Fake { dir }
        }

        fn tool(path: &Path, text: &str) {
            fs::write(path, text).unwrap();
            chmod(path, 0o755);
        }

        /// `entry`'s script under `accounts`, run with the stub getent first
        /// on `PATH`: its exit code, stdout, stderr and the tools' log.
        fn run(
            &self,
            entry: &GroupEntry,
            accounts: Accounts,
        ) -> (Option<i32>, String, String, String) {
            let bin = self.dir.join("bin");
            let script = format!(
                "PATH='{}':$PATH\n{}",
                self.dir.display(),
                script("groups[0]", entry, (accounts, bin.to_str().unwrap()), "")
            );
            let out = Local.run(&script, RUN_TIMEOUT).unwrap();
            let log = fs::read_to_string(self.dir.join("log")).unwrap_or_default();
            (
                out.code,
                String::from_utf8(out.stdout).unwrap(),
                String::from_utf8(out.stderr).unwrap(),
                log,
            )
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    const PASSWD: &str = "root:x:0:0::/root:/bin/sh\ndeploy:x:1000:2000::/home/deploy:/bin/sh\n";

    #[test]
    fn the_shadow_script_creates_fixes_and_removes_with_shadow_tools() {
        let script = script(
            "groups[0]",
            &entry("gid = 2000\nsystem = true"),
            (Accounts::Shadow, "/usr/sbin"),
            "sudo -n ",
        );
        for wanted in [
            "sudo -n true || exit\n",
            "anvil_line=$(anvil_get group app 1); anvil_rc=$?\n",
            "anvil_held=$(anvil_get group 2000 3) && { echo 'groups[0] app: gid 2000 is held by group '\"${anvil_held%%:*}\" >&2; exit 1; }\n",
            "sudo -n '/usr/sbin'/groupadd -g 2000 -r app >&2 || exit\necho 'created group app'\n",
            "elif [ \"$anvil_gid\" != 2000 ]; then\n",
            "sudo -n '/usr/sbin'/groupmod -g 2000 app >&2 || exit\necho 'fixed group app: gid'\n",
            "else echo 'unchanged group app'\nfi\n",
        ] {
            assert!(script.contains(wanted), "{wanted}\n---\n{script}");
        }
        let absent = super::script(
            "tasks[2].group",
            &entry("state = \"absent\""),
            (Accounts::Shadow, "/usr/sbin"),
            "",
        );
        assert!(!absent.contains("true || exit"), "{absent}");
        for wanted in [
            "'tasks[2].group app: cannot remove the primary group of user '\"$anvil_user\"",
            "'/usr/sbin'/groupdel app >&2 || exit\necho 'removed group app'\nfi\n",
            "if [ -z \"$anvil_line\" ]; then echo 'unchanged group app'\n",
        ] {
            assert!(absent.contains(wanted), "{wanted}\n---\n{absent}");
        }
    }

    #[test]
    fn the_busybox_script_uses_addgroup_and_delgroup_and_refuses_a_gid_change() {
        let present = script(
            "groups[1]",
            &entry("gid = 2000\nsystem = true"),
            (Accounts::Busybox, "/usr/sbin"),
            "",
        );
        for wanted in [
            "'/usr/sbin'/addgroup -g 2000 -S app >&2 || exit\necho 'created group app'\n",
            "elif [ \"$anvil_gid\" != 2000 ]; then { echo 'groups[1] app: busybox cannot change gid; install shadow' >&2; exit 1; }\n",
        ] {
            assert!(present.contains(wanted), "{wanted}\n---\n{present}");
        }
        assert!(!present.contains("groupmod"), "{present}");
        let plain = script("groups[1]", &entry(""), (Accounts::Busybox, "/bin"), "");
        assert!(plain.contains("'/bin'/addgroup app >&2"), "{plain}");
        assert!(!plain.contains("elif"), "{plain}");
        let absent = script(
            "groups[1]",
            &entry("state = \"absent\""),
            (Accounts::Busybox, "/usr/sbin"),
            "",
        );
        assert!(
            absent.contains("'/usr/sbin'/delgroup app >&2 || exit\n"),
            "{absent}"
        );
    }

    #[test]
    fn a_missing_group_is_created_and_an_existing_one_is_unchanged() {
        for accounts in [Accounts::Shadow, Accounts::Busybox] {
            let fake = Fake::new("root:x:0:\n", PASSWD);
            let (code, out, _, log) = fake.run(&entry("gid = 2000"), accounts);
            assert_eq!((code, out.as_str()), (Some(0), "created group app\n"));
            let tool = if accounts == Accounts::Shadow {
                "groupadd"
            } else {
                "addgroup"
            };
            assert_eq!(log, format!("{tool} -g 2000 app\n"));
            let fake = Fake::new("root:x:0:\napp:x:2000:deploy\n", PASSWD);
            for keys in ["gid = 2000", ""] {
                let (code, out, _, log) = fake.run(&entry(keys), accounts);
                assert_eq!(
                    (code, out.as_str(), log.as_str()),
                    (Some(0), "unchanged group app\n", "")
                );
            }
        }
    }

    #[test]
    fn a_different_gid_is_fixed_on_shadow_and_refused_on_busybox() {
        let fake = Fake::new("app:x:1500:\n", PASSWD);
        let (code, out, _, log) = fake.run(&entry("gid = 2000"), Accounts::Shadow);
        assert_eq!(
            (code, out.as_str(), log.as_str()),
            (Some(0), "fixed group app: gid\n", "groupmod -g 2000 app\n")
        );
        let fake = Fake::new("app:x:1500:\n", PASSWD);
        let (code, out, err, log) = fake.run(&entry("gid = 2000"), Accounts::Busybox);
        assert_eq!((code, out.as_str(), log.as_str()), (Some(1), "", ""));
        assert_eq!(
            err,
            "groups[0] app: busybox cannot change gid; install shadow\n"
        );
    }

    #[test]
    fn a_gid_another_group_holds_fails_naming_both() {
        for group in ["web:x:2000:\n", "app:x:1500:\nweb:x:2000:\n"] {
            let fake = Fake::new(group, PASSWD);
            let (code, out, err, log) = fake.run(&entry("gid = 2000"), Accounts::Shadow);
            assert_eq!(
                (code, out.as_str(), log.as_str()),
                (Some(1), "", ""),
                "{group}"
            );
            assert_eq!(err, "groups[0] app: gid 2000 is held by group web\n");
        }
    }

    #[test]
    fn an_absent_group_is_removed_unless_it_is_a_primary_group() {
        let absent = entry("state = \"absent\"");
        for accounts in [Accounts::Shadow, Accounts::Busybox] {
            let fake = Fake::new("app:x:3000:\n", PASSWD);
            let (code, out, _, log) = fake.run(&absent, accounts);
            let tool = if accounts == Accounts::Shadow {
                "groupdel"
            } else {
                "delgroup"
            };
            assert_eq!((code, out.as_str()), (Some(0), "removed group app\n"));
            assert_eq!(log, format!("{tool} app\n"));
            let fake = Fake::new("root:x:0:\n", PASSWD);
            let (code, out, _, log) = fake.run(&absent, accounts);
            assert_eq!(
                (code, out.as_str(), log.as_str()),
                (Some(0), "unchanged group app\n", "")
            );
            let fake = Fake::new("app:x:2000:\n", PASSWD);
            let (code, _, err, log) = fake.run(&absent, accounts);
            assert_eq!((code, log.as_str()), (Some(1), ""));
            assert_eq!(
                err,
                "groups[0] app: cannot remove the primary group of user deploy\n"
            );
        }
    }

    #[test]
    fn accounts_come_from_the_facts_and_need_an_absolute_bin() {
        assert_eq!(
            Accounts::of(&facts("accounts=shadow\naccounts_bin=/usr/sbin\n")),
            Some((Accounts::Shadow, "/usr/sbin"))
        );
        assert_eq!(
            Accounts::of(&facts("accounts=busybox\naccounts_bin=/bin\n")),
            Some((Accounts::Busybox, "/bin"))
        );
        for text in [
            "accounts=none\naccounts_bin=\n",
            "accounts=shadow\naccounts_bin=sbin\n",
            "accounts=shadow\n",
            "",
        ] {
            assert_eq!(Accounts::of(&facts(text)), None, "{text}");
        }
    }

    #[test]
    fn the_shown_line_comes_from_the_config_alone() {
        assert_eq!(shown(&entry("")), "group app");
        assert_eq!(shown(&entry("gid = 2000")), "group app (gid 2000)");
        assert_eq!(
            shown(&entry("gid = 2000\nsystem = true")),
            "group app (gid 2000, system)"
        );
        assert_eq!(shown(&entry("state = \"absent\"")), "remove group app");
    }
}
