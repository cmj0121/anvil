//! `[[users]]`: a local user created, or the keys set on it fixed, on each
//! target whose tags and facts all match.
//!
//! [`run`] turns each entry into one step per target, as `[[groups]]` does:
//! a skip when the entry does not match, else one script, run as root or
//! through sudo, with the tools from the probe's `accounts` and
//! `accounts_bin` facts called by absolute path; a target with neither
//! shadow-utils nor busybox fails the step.
//!
//! [`script`] reads the user with `getent passwd` (awk over `/etc/passwd`
//! without getent) and `id -Gn`. Before any change it fails the step when a
//! `group` or `groups` entry is missing on the target, the `shell` is not in
//! `/etc/shells`, the `uid` is another user's, or busybox would have to
//! change a field it cannot. It then compares and fixes only the keys that
//! are set: it prints `created user NAME`, `fixed user NAME: shell, groups`
//! (naming what changed) or `unchanged user NAME`. A changed `home` is not
//! moved: a warning says so and only the passwd field is set. The
//! `[[groups]]` of the same run come first, so a group they create counts.
//! Nothing is recorded for rollback.

use crate::config::{Config, UserEntry, is_account_name};
use crate::facts::Facts;
use crate::files::sudo_secret_for;
use crate::groups::Accounts;
use crate::inventory::Target;
use crate::remote::quote;
use crate::step::{PlannedRun, RunKind, UserRun};
use crate::tasks::Item;
use crate::transport::RUN_TIMEOUT;

/// The failure of a step on a target with no account tools.
pub const NO_TOOLS: &str = "users: no account tools on this target (accounts = none)";

/// The step of user `item`, holding `entry`, the `i`th user item.
pub fn run(
    config: &Config,
    item: &Item,
    (entry, i): (&UserEntry, usize),
    target: &Target,
    facts: Option<&Facts>,
) -> Result<PlannedRun, String> {
    let facts = facts.ok_or("users: the probe has not run")?;
    let label = item.label("users", i);
    let name = &entry.name;
    if !item.matches(target, facts) {
        log::info!("{}: {label} {name}: no match", target.name);
        return Ok(user_run(
            name,
            String::new(),
            format!("skipped user {name}: no match"),
            None,
            true,
        ));
    }
    let Some((accounts, bin)) = Accounts::of(facts) else {
        log::info!("{}: {label} {name}: no account tools", target.name);
        return Ok(user_run(
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
    Ok(user_run(
        name,
        script(&label, entry, (accounts, bin), sudo),
        shown(entry),
        secret.map(str::to_owned),
        false,
    ))
}

/// What the step shows, from the config alone: `user NAME`, with `(uid N,
/// group G, groups A B, shell PATH, home PATH, system, no home)` for those
/// set; `only groups` when `exclusive_groups` is, `no groups` for an empty
/// exclusive list, and nothing for an empty list that changes nothing.
pub fn shown(entry: &UserEntry) -> String {
    let name = &entry.name;
    let groups = match (entry.groups.as_deref(), entry.exclusive_groups) {
        (None, _) | (Some([]), false) => None,
        (Some([]), true) => Some("no groups".to_owned()),
        (Some(groups), true) => Some(format!("only groups {}", groups.join(" "))),
        (Some(groups), false) => Some(format!("groups {}", groups.join(" "))),
    };
    let attrs: Vec<String> = [
        entry.uid.map(|uid| format!("uid {uid}")),
        entry.group.as_ref().map(|group| format!("group {group}")),
        groups,
        entry.shell.as_ref().map(|shell| format!("shell {shell}")),
        entry.home.as_ref().map(|home| format!("home {home}")),
        entry.system.then(|| "system".to_owned()),
        (!entry.create_home).then(|| "no home".to_owned()),
    ]
    .into_iter()
    .flatten()
    .collect();
    match attrs.as_slice() {
        [] => format!("user {name}"),
        attrs => format!("user {name} ({})", attrs.join(", ")),
    }
}

fn user_run(
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
        kind: RunKind::User {
            name: name.to_owned(),
            run: UserRun { skip },
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

/// One sh brace group that brings user `entry` to the keys it sets with the
/// `accounts` tools in `bin`; `label` names the entry in its failures and
/// `sudo` is "" (root), "sudo -n " or "sudo " (a password typed). Reads need
/// no root; with `sudo` set, `sudo true` runs first. Names are inserted
/// bare: their charset needs no quoting and they never start with `-`;
/// paths and the comment are quoted. Every check runs before any change,
/// and the flags of a shadow fix gather in `"$@"` for one `usermod`. The
/// tools' own output goes to stderr, so stdout holds only the script's line.
pub fn script(
    label: &str,
    entry: &UserEntry,
    (accounts, bin): (Accounts, &str),
    sudo: &str,
) -> String {
    let name = entry.name.as_str();
    debug_assert!(is_account_name(name), "{name}");
    let busybox = accounts == Accounts::Busybox;
    let bin = quote(bin);
    let at = format!("{label} {name}");
    let line = |text: String| format!("echo {}\n", quote(&text));
    let check = if sudo.is_empty() {
        String::new()
    } else {
        format!("{sudo}true || exit\n")
    };
    let mut checks = String::new();
    if let Some(group) = &entry.group {
        let field = if group.bytes().all(|b| b.is_ascii_digit()) {
            3
        } else {
            1
        };
        checks += &format!(
            "anvil_pl=$(anvil_get group {group} {field}) || {}\n\
             anvil_pname=${{anvil_pl%%:*}}; anvil_pgid=${{anvil_pl#*:*:}}; \
             anvil_pgid=${{anvil_pgid%%:*}}\n",
            fail(&format!(
                "{at}: group {group} does not exist on this target"
            )),
        );
    }
    let groups = entry.groups.as_deref();
    for group in groups.into_iter().flatten() {
        checks += &format!(
            "anvil_get group {group} 1 >/dev/null || {}\n",
            fail(&format!(
                "{at}: group {group} does not exist on this target"
            )),
        );
    }
    if let Some(shell) = &entry.shell {
        checks += &format!(
            "grep -qxF -e {} /etc/shells 2>/dev/null || {}\n",
            quote(shell),
            fail(&format!("{at}: shell {shell} is not in /etc/shells")),
        );
    }
    if let Some(uid) = entry.uid {
        checks += &format!(
            "anvil_held=$(anvil_get passwd {uid} 3) && [ \"${{anvil_held%%:*}}\" != {name} ] && \
             {{ echo {}\"${{anvil_held%%:*}}\" >&2; exit 1; }}\n",
            quote(&format!("{at}: uid {uid} is held by user ")),
        );
    }

    let create = create(entry, (accounts, &bin), sudo);
    let created = line(format!("created user {name}"));

    // An existing user: each set key compared, its fix gathered or refused.
    let mut fix = String::new();
    let mut field = |key: &str, differs: String, flag: String| {
        fix += &if busybox {
            format!(
                "{differs} && {}\n",
                fail(&format!(
                    "{at}: busybox cannot change {key}; install shadow"
                ))
            )
        } else {
            format!(
                "{differs} && {{ set -- \"$@\" {flag}; anvil_fixed=\"$anvil_fixed, {key}\"; }}\n"
            )
        };
    };
    if let Some(uid) = entry.uid {
        field(
            "uid",
            format!("[ \"$anvil_uid\" != {uid} ]"),
            format!("-u {uid}"),
        );
    }
    if entry.group.is_some() {
        field(
            "group",
            "[ \"$anvil_gid\" != \"$anvil_pgid\" ]".to_owned(),
            "-g \"$anvil_pgid\"".to_owned(),
        );
    }
    if let Some(shell) = &entry.shell {
        let shell = quote(shell);
        field(
            "shell",
            format!("[ \"$anvil_shell\" != {shell} ]"),
            format!("-s {shell}"),
        );
    }
    if let Some(home) = &entry.home {
        let quoted = quote(home);
        let warn = format!(
            "{{ echo {}\"$anvil_home\" >&2; }}",
            quote(&format!(
                "warning: {at}: home set to {home}; nothing is moved from "
            ))
        );
        let differs = format!("[ \"$anvil_home\" != {quoted} ]");
        field(
            "home",
            if busybox {
                differs
            } else {
                format!("{differs} && {warn}")
            },
            format!("-d {quoted}"),
        );
    }
    if let Some(comment) = &entry.comment {
        let comment = quote(comment);
        field(
            "comment",
            format!("[ \"$anvil_gecos\" != {comment} ]"),
            format!("-c {comment}"),
        );
    }
    let mut apply = String::new();
    if let Some(groups) = groups {
        let list = groups.join(" ");
        fix += &format!(
            "anvil_have=$(id -Gn {name}) && anvil_cur=$(id -gn {name}) || {}\n\
             anvil_add=; anvil_del=\n",
            fail(&format!("users: cannot read the groups of user {name}")),
        );
        for group in groups {
            fix += &format!(
                "case \" $anvil_have \" in *\" {group} \"*) ;; *) anvil_add=\"$anvil_add {group}\" ;; esac\n"
            );
        }
        if entry.exclusive_groups {
            fix += &format!(
                "for anvil_x in $anvil_have; do case \" {list} \" in *\" $anvil_x \"*) ;; *) \
                 [ \"$anvil_x\" = \"$anvil_cur\" ] || anvil_del=\"$anvil_del $anvil_x\" ;; esac; done\n"
            );
        }
        let changed = "[ -n \"$anvil_add$anvil_del\" ]";
        if busybox {
            fix += &format!("{changed} && anvil_fixed=\"$anvil_fixed, groups\"\n");
            apply = format!(
                "for anvil_x in $anvil_add; do {sudo}{bin}/addgroup {name} \"$anvil_x\" >&2 || exit; done\n\
                 for anvil_x in $anvil_del; do {sudo}{bin}/delgroup {name} \"$anvil_x\" >&2 || exit; done\n"
            );
        } else {
            let append = if entry.exclusive_groups { "" } else { "-a " };
            fix += &format!(
                "{changed} && {{ set -- \"$@\" {append}-G {}; anvil_fixed=\"$anvil_fixed, groups\"; }}\n",
                quote(&groups.join(","))
            );
        }
    }
    if !busybox {
        apply = format!("[ $# -eq 0 ] || {sudo}{bin}/usermod \"$@\" {name} >&2 || exit\n");
    }
    let unchanged = line(format!("unchanged user {name}"));
    format!(
        "{{ LC_ALL=C; export LC_ALL; set -f\n\
         {check}\
         anvil_g=; command -v getent >/dev/null 2>&1 && anvil_g=1\n\
         anvil_get() {{ if [ -n \"$anvil_g\" ]; then getent \"$1\" \"$2\"; else \
         awk -F: -v k=\"$2\" -v f=\"$3\" '$f == k {{ print; n = 1; exit }} END {{ exit n ? 0 : 2 }}' \
         \"/etc/$1\"; fi; }}\n\
         anvil_line=$(anvil_get passwd {name} 1); anvil_rc=$?\n\
         case $anvil_rc in 0) ;; 2) anvil_line= ;; *) {unread} ;; esac\n\
         {checks}\
         if [ -z \"$anvil_line\" ]; then\n{create}{created}else\n\
         anvil_r=${{anvil_line#*:*:}}; anvil_uid=${{anvil_r%%:*}}; anvil_r=${{anvil_r#*:}}\n\
         anvil_gid=${{anvil_r%%:*}}; anvil_r=${{anvil_r#*:}}; anvil_gecos=${{anvil_r%%:*}}\n\
         anvil_r=${{anvil_r#*:}}; anvil_home=${{anvil_r%%:*}}; anvil_shell=${{anvil_r#*:}}\n\
         anvil_fixed=; set --\n\
         {fix}\
         if [ -n \"$anvil_fixed\" ]; then\n{apply}\
         echo {fixed}\"${{anvil_fixed#, }}\"\n\
         else {unchanged}fi\nfi\n}}\n",
        unread = fail(&format!("users: cannot read user {name}")),
        fixed = quote(&format!("fixed user {name}: ")),
    )
}

/// The commands that create user `entry`, then add a busybox user to its
/// supplementary groups; a numeric `group` is passed as the name the checks
/// found for it.
fn create(entry: &UserEntry, (accounts, bin): (Accounts, &str), sudo: &str) -> String {
    let name = entry.name.as_str();
    let (system, no_home) = match accounts {
        Accounts::Shadow => ("-r", "-M"),
        Accounts::Busybox => ("-S", "-H"),
    };
    let shell = entry
        .shell
        .as_ref()
        .map(|shell| format!("-s {}", quote(shell)));
    let home = match accounts {
        Accounts::Shadow => format!("-d {}", quote(&entry.new_home())),
        Accounts::Busybox => format!("-h {}", quote(&entry.new_home())),
    };
    let comment = |flag: &str| (entry.comment.as_ref()).map(|c| format!("{flag} {}", quote(c)));
    let flags: Vec<String> = match accounts {
        Accounts::Shadow => vec![
            entry.uid.map(|uid| format!("-u {uid}")),
            entry
                .group
                .as_ref()
                .map(|_| "-g \"$anvil_pname\"".to_owned()),
            (entry.groups.as_ref())
                .filter(|groups| !groups.is_empty())
                .map(|groups| format!("-G {}", quote(&groups.join(",")))),
            shell,
            Some(home),
            Some(if entry.create_home { "-m" } else { no_home }.to_owned()),
            entry.system.then(|| system.to_owned()),
            comment("-c"),
        ],
        Accounts::Busybox => vec![
            Some("-D".to_owned()),
            entry.uid.map(|uid| format!("-u {uid}")),
            entry
                .group
                .as_ref()
                .map(|_| "-G \"$anvil_pname\"".to_owned()),
            shell,
            Some(home),
            (!entry.create_home).then(|| no_home.to_owned()),
            entry.system.then(|| system.to_owned()),
            comment("-g"),
        ],
    }
    .into_iter()
    .flatten()
    .collect();
    let tool = match accounts {
        Accounts::Shadow => "useradd",
        Accounts::Busybox => "adduser",
    };
    let mut text = format!(
        "{sudo}{bin}/{tool} {} {name} >&2 || exit\n",
        flags.join(" ")
    );
    if accounts == Accounts::Busybox {
        for group in entry.groups.iter().flatten() {
            text += &format!("{sudo}{bin}/addgroup {name} {group} >&2 || exit\n");
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::test_util::{chmod, temp_dir};
    use crate::transport::{Local, Transport};

    fn entry(keys: &str) -> UserEntry {
        toml::from_str(&format!("name = \"deploy\"\n{keys}")).unwrap()
    }

    const GROUP: &str = "root:x:0:\nusers:x:100:\napp:x:2000:\ndocker:x:2001:deploy\n\
                         wheel:x:2002:deploy\naudio:x:2003:\n";
    const PASSWD: &str = "root:x:0:0::/root:/bin/sh\n\
                          deploy:x:1500:100:Deploy:/home/deploy:/bin/sh\n";
    const SHELLS: &str = "/bin/sh\n/bin/bash\n";

    /// A stub `getent` and `id` over `group` and `passwd` files in `dir`, an
    /// `/etc/shells` stand-in at `dir/shells`, and stub account tools in
    /// `dir/bin` that log their arguments to `dir/log`.
    struct Fake {
        dir: PathBuf,
    }

    impl Fake {
        fn new(passwd: &str) -> Fake {
            let dir = temp_dir("users");
            fs::create_dir(dir.join("bin")).unwrap();
            fs::write(dir.join("group"), GROUP).unwrap();
            fs::write(dir.join("passwd"), passwd).unwrap();
            fs::write(dir.join("shells"), SHELLS).unwrap();
            let d = dir.display();
            let getent = format!(
                "#!/bin/sh\n\
                 awk -F: -v k=\"$2\" '$1 == k || $3 == k {{ print; f = 1 }} END {{ exit f ? 0 : 2 }}' \
                 '{d}'/\"$1\"\n"
            );
            Self::tool(&dir.join("getent"), &getent);
            let id = format!(
                "#!/bin/sh\nu=$2\n\
                 g=$(awk -F: -v u=\"$u\" '$1 == u {{ print $4 }}' '{d}'/passwd)\n\
                 p=$(awk -F: -v g=\"$g\" '$3 == g {{ print $1 }}' '{d}'/group)\n\
                 [ \"$1\" = -gn ] && {{ echo \"$p\"; exit; }}\n\
                 s=$(awk -F: -v u=\"$u\" '{{ n = split($4, m, \",\"); for (i = 1; i <= n; i++) \
                 if (m[i] == u) printf \" %s\", $1 }}' '{d}'/group)\n\
                 echo \"$p$s\"\n"
            );
            Self::tool(&dir.join("id"), &id);
            let grep = format!(
                "#!/bin/sh\n[ \"$4\" = /etc/shells ] && set -- \"$1\" \"$2\" \"$3\" '{d}'/shells\n\
                 exec /usr/bin/grep \"$@\"\n"
            );
            Self::tool(&dir.join("grep"), &grep);
            for tool in ["useradd", "usermod", "adduser", "addgroup", "delgroup"] {
                let log = format!("#!/bin/sh\necho \"{tool} $*\" >>'{d}'/log\necho {tool} ran\n");
                Self::tool(&dir.join("bin").join(tool), &log);
            }
            Fake { dir }
        }

        fn tool(path: &Path, text: &str) {
            fs::write(path, text).unwrap();
            chmod(path, 0o755);
        }

        /// `entry`'s script under `accounts`, run with the stubs first on
        /// `PATH`: its exit code, stdout, stderr and the tools' log.
        fn run(
            &self,
            entry: &UserEntry,
            accounts: Accounts,
        ) -> (Option<i32>, String, String, String) {
            let bin = self.dir.join("bin");
            let script = format!(
                "PATH='{}':$PATH\n{}",
                self.dir.display(),
                script("users[0]", entry, (accounts, bin.to_str().unwrap()), "")
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

    const ALL: &str = "uid = 2000\ngroup = \"app\"\ngroups = [\"docker\", \"wheel\"]\n\
                       shell = \"/bin/bash\"\nhome = \"/srv/deploy\"\nsystem = true\n\
                       create_home = false\ncomment = \"Deploy bot\"";

    #[test]
    fn the_shadow_script_creates_with_useradd_and_fixes_with_one_usermod() {
        let script = script(
            "users[0]",
            &entry(ALL),
            (Accounts::Shadow, "/usr/sbin"),
            "sudo -n ",
        );
        for wanted in [
            "sudo -n true || exit\n",
            "anvil_line=$(anvil_get passwd deploy 1); anvil_rc=$?\n",
            "anvil_pl=$(anvil_get group app 1) || { echo 'users[0] deploy: group app does not exist on this target' >&2; exit 1; }\n",
            "anvil_get group docker 1 >/dev/null || { echo 'users[0] deploy: group docker does not exist on this target' >&2; exit 1; }\n",
            "grep -qxF -e '/bin/bash' /etc/shells 2>/dev/null || { echo 'users[0] deploy: shell /bin/bash is not in /etc/shells' >&2; exit 1; }\n",
            "sudo -n '/usr/sbin'/useradd -u 2000 -g \"$anvil_pname\" -G 'docker,wheel' -s '/bin/bash' -d '/srv/deploy' -M -r -c 'Deploy bot' deploy >&2 || exit\necho 'created user deploy'\n",
            "[ \"$anvil_shell\" != '/bin/bash' ] && { set -- \"$@\" -s '/bin/bash'; anvil_fixed=\"$anvil_fixed, shell\"; }\n",
            "[ -n \"$anvil_add$anvil_del\" ] && { set -- \"$@\" -a -G 'docker,wheel'; anvil_fixed=\"$anvil_fixed, groups\"; }\n",
            "[ $# -eq 0 ] || sudo -n '/usr/sbin'/usermod \"$@\" deploy >&2 || exit\n",
            "else echo 'unchanged user deploy'\nfi\n",
        ] {
            assert!(script.contains(wanted), "{wanted}\n---\n{script}");
        }
        assert!(!script.contains("adduser"), "{script}");
    }

    #[test]
    fn the_busybox_script_uses_adduser_and_addgroup_and_refuses_other_fixes() {
        let script = script(
            "users[1]",
            &entry(ALL),
            (Accounts::Busybox, "/usr/sbin"),
            "",
        );
        for wanted in [
            "'/usr/sbin'/adduser -D -u 2000 -G \"$anvil_pname\" -s '/bin/bash' -h '/srv/deploy' -H -S -g 'Deploy bot' deploy >&2 || exit\n\
             '/usr/sbin'/addgroup deploy docker >&2 || exit\n'/usr/sbin'/addgroup deploy wheel >&2 || exit\n\
             echo 'created user deploy'\n",
            "[ \"$anvil_shell\" != '/bin/bash' ] && { echo 'users[1] deploy: busybox cannot change shell; install shadow' >&2; exit 1; }\n",
            "for anvil_x in $anvil_add; do '/usr/sbin'/addgroup deploy \"$anvil_x\" >&2 || exit; done\n",
        ] {
            assert!(script.contains(wanted), "{wanted}\n---\n{script}");
        }
        assert!(!script.contains("usermod"), "{script}");
        assert!(!script.contains("true || exit"), "{script}");
    }

    #[test]
    fn a_missing_user_is_created_with_the_set_keys_and_a_default_home() {
        let fake = Fake::new("root:x:0:0::/root:/bin/sh\n");
        let keys = "group = \"2000\"\ngroups = [\"docker\"]\nshell = \"/bin/bash\"";
        let (code, out, err, log) = fake.run(&entry(keys), Accounts::Shadow);
        assert_eq!(
            (code, out.as_str()),
            (Some(0), "created user deploy\n"),
            "{err}"
        );
        assert_eq!(
            log,
            "useradd -g app -G docker -s /bin/bash -d /home/deploy -m deploy\n"
        );
        let fake = Fake::new("root:x:0:0::/root:/bin/sh\n");
        let (code, out, err, log) = fake.run(&entry(keys), Accounts::Busybox);
        assert_eq!(
            (code, out.as_str()),
            (Some(0), "created user deploy\n"),
            "{err}"
        );
        assert_eq!(
            log,
            "adduser -D -G app -s /bin/bash -h /home/deploy deploy\naddgroup deploy docker\n"
        );
    }

    #[test]
    fn unset_keys_leave_an_existing_user_alone() {
        for accounts in [Accounts::Shadow, Accounts::Busybox] {
            let fake = Fake::new(PASSWD);
            for keys in ["", "create_home = false\nsystem = true"] {
                let (code, out, err, log) = fake.run(&entry(keys), accounts);
                assert_eq!(
                    (code, out.as_str(), log.as_str()),
                    (Some(0), "unchanged user deploy\n", ""),
                    "{err}"
                );
            }
            let same = "uid = 1500\ngroup = \"users\"\ngroups = [\"wheel\"]\n\
                        shell = \"/bin/sh\"\nhome = \"/home/deploy\"\ncomment = \"Deploy\"";
            let (code, out, err, log) = fake.run(&entry(same), accounts);
            assert_eq!(
                (code, out.as_str(), log.as_str()),
                (Some(0), "unchanged user deploy\n", ""),
                "{err}"
            );
        }
    }

    #[test]
    fn shadow_fixes_only_what_differs_in_one_usermod_naming_it() {
        let fake = Fake::new(PASSWD);
        let keys = "uid = 1500\nshell = \"/bin/bash\"\ngroups = [\"docker\", \"audio\"]";
        let (code, out, err, log) = fake.run(&entry(keys), Accounts::Shadow);
        assert_eq!(
            (code, out.as_str(), err.as_str()),
            (
                Some(0),
                "fixed user deploy: shell, groups\n",
                "usermod ran\n"
            )
        );
        assert_eq!(log, "usermod -s /bin/bash -a -G docker,audio deploy\n");
        let keys = "group = \"app\"\nhome = \"/srv/deploy\"\ncomment = \"Bot\"";
        let (code, out, err, log) = Fake::new(PASSWD).run(&entry(keys), Accounts::Shadow);
        assert_eq!(
            (code, out.as_str()),
            (Some(0), "fixed user deploy: group, home, comment\n")
        );
        assert_eq!(log, "usermod -g 2000 -d /srv/deploy -c Bot deploy\n");
        assert!(
            err.starts_with(
                "warning: users[0] deploy: home set to /srv/deploy; nothing is moved from /home/deploy\n"
            ),
            "{err}"
        );
    }

    #[test]
    fn listed_groups_are_added_and_exclusive_groups_also_removes_the_rest() {
        let add = "groups = [\"audio\"]";
        let exclusive = "groups = [\"audio\", \"docker\"]\nexclusive_groups = true";
        let fake = Fake::new(PASSWD);
        let (_, out, _, log) = fake.run(&entry(add), Accounts::Shadow);
        assert_eq!(out, "fixed user deploy: groups\n");
        assert_eq!(log, "usermod -a -G audio deploy\n");
        let fake = Fake::new(PASSWD);
        let (_, out, _, log) = fake.run(&entry(exclusive), Accounts::Shadow);
        assert_eq!(out, "fixed user deploy: groups\n");
        assert_eq!(log, "usermod -G audio,docker deploy\n");
        let fake = Fake::new(PASSWD);
        let (_, out, _, log) = fake.run(&entry(add), Accounts::Busybox);
        assert_eq!(out, "fixed user deploy: groups\n");
        assert_eq!(log, "addgroup deploy audio\n");
        let fake = Fake::new(PASSWD);
        let (_, out, _, log) = fake.run(&entry(exclusive), Accounts::Busybox);
        assert_eq!(out, "fixed user deploy: groups\n");
        assert_eq!(log, "addgroup deploy audio\ndelgroup deploy wheel\n");
        let (_, out, _, log) = Fake::new(PASSWD).run(
            &entry("groups = [\"docker\", \"wheel\"]\nexclusive_groups = true"),
            Accounts::Busybox,
        );
        assert_eq!(
            (out.as_str(), log.as_str()),
            ("unchanged user deploy\n", "")
        );
    }

    #[test]
    fn exclusive_groups_drop_a_supplementary_membership_in_the_new_primary_group() {
        let keys = "group = \"docker\"\ngroups = [\"wheel\"]\nexclusive_groups = true";
        let (code, out, _, log) = Fake::new(PASSWD).run(&entry(keys), Accounts::Shadow);
        assert_eq!(
            (code, out.as_str(), log.as_str()),
            (
                Some(0),
                "fixed user deploy: group, groups\n",
                "usermod -g 2001 -G wheel deploy\n"
            )
        );
        let fixed = "root:x:0:0::/root:/bin/sh\ndeploy:x:1500:2001:Deploy:/home/deploy:/bin/sh\n";
        let fake = Fake::new(fixed);
        fs::write(
            fake.dir.join("group"),
            GROUP.replace("docker:x:2001:deploy", "docker:x:2001:"),
        )
        .unwrap();
        let (code, out, _, log) = fake.run(&entry(keys), Accounts::Shadow);
        assert_eq!(
            (code, out.as_str(), log.as_str()),
            (Some(0), "unchanged user deploy\n", "")
        );
    }

    #[test]
    fn busybox_refuses_to_change_a_field_naming_it_before_any_change() {
        for (keys, key) in [
            ("uid = 2000", "uid"),
            ("group = \"app\"", "group"),
            ("shell = \"/bin/bash\"\ngroups = [\"audio\"]", "shell"),
            ("home = \"/srv/deploy\"", "home"),
            ("comment = \"Bot\"", "comment"),
        ] {
            let (code, out, err, log) = Fake::new(PASSWD).run(&entry(keys), Accounts::Busybox);
            assert_eq!(
                (code, out.as_str(), log.as_str()),
                (Some(1), "", ""),
                "{keys}"
            );
            assert_eq!(
                err,
                format!("users[0] deploy: busybox cannot change {key}; install shadow\n")
            );
        }
    }

    #[test]
    fn a_missing_group_a_foreign_shell_or_a_held_uid_fails_before_any_change() {
        for (keys, wanted) in [
            (
                "group = \"nope\"",
                "users[0] deploy: group nope does not exist on this target\n",
            ),
            (
                "group = \"4242\"",
                "users[0] deploy: group 4242 does not exist on this target\n",
            ),
            (
                "groups = [\"docker\", \"nope\"]",
                "users[0] deploy: group nope does not exist on this target\n",
            ),
            (
                "shell = \"/bin/zsh\"",
                "users[0] deploy: shell /bin/zsh is not in /etc/shells\n",
            ),
            ("uid = 0", "users[0] deploy: uid 0 is held by user root\n"),
        ] {
            for passwd in [PASSWD, "root:x:0:0::/root:/bin/sh\n"] {
                let (code, out, err, log) = Fake::new(passwd).run(&entry(keys), Accounts::Shadow);
                assert_eq!(
                    (code, out.as_str(), log.as_str()),
                    (Some(1), "", ""),
                    "{keys}"
                );
                assert_eq!(err, wanted, "{keys}");
            }
        }
    }

    #[test]
    fn the_shown_line_comes_from_the_config_alone() {
        assert_eq!(shown(&entry("")), "user deploy");
        assert_eq!(
            shown(&entry(
                "uid = 2000\ngroup = \"app\"\ngroups = [\"docker\"]\nshell = \"/bin/bash\""
            )),
            "user deploy (uid 2000, group app, groups docker, shell /bin/bash)"
        );
        assert_eq!(
            shown(&entry(
                "groups = [\"a\", \"b\"]\nexclusive_groups = true\nhome = \"/srv/d\"\n\
                 system = true\ncreate_home = false"
            )),
            "user deploy (only groups a b, home /srv/d, system, no home)"
        );
        assert_eq!(
            shown(&entry("groups = []\nexclusive_groups = true")),
            "user deploy (no groups)"
        );
        assert_eq!(
            shown(&entry("groups = []\nshell = \"/bin/sh\"")),
            "user deploy (shell /bin/sh)"
        );
    }
}
