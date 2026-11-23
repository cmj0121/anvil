//! `[[users]]`: a local user created, the keys set on it fixed, or removed, on each
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
//! A `state = "absent"` entry runs `userdel` (busybox `deluser`) and prints
//! `removed user NAME`, or `unchanged user NAME` when it is gone. Nothing is
//! recorded for rollback.
//!
//! Before any step on a target, the lockout guards fail it: [`check_config`]
//! from the config and the probe's `user` fact, then [`check_state`] from
//! the read-only [`query`], as the target was before the run.

use std::collections::BTreeMap;

use crate::config::{Config, UserEntry, is_account_name};
use crate::facts::Facts;
use crate::files::sudo_secret_for;
use crate::groups::Accounts;
use crate::inventory::Target;
use crate::remote::quote;
use crate::step::{PlannedRun, RunKind, UserRun};
use crate::tasks::{Expanded, Item, ItemKind};
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
/// exclusive list, and nothing for an empty list that changes nothing; or
/// `remove user NAME`.
pub fn shown(entry: &UserEntry) -> String {
    let name = &entry.name;
    if entry.absent() {
        return format!("remove user {name}");
    }
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
    let unchanged = line(format!("unchanged user {name}"));
    if entry.absent() {
        let delete = match accounts {
            Accounts::Shadow => format!(
                "{sudo}{bin}/userdel{}{} {name}",
                if entry.force { " -f" } else { "" },
                if entry.remove_home { " -r" } else { "" },
            ),
            Accounts::Busybox => format!(
                "{sudo}{bin}/deluser{} {name}",
                if entry.remove_home {
                    " --remove-home"
                } else {
                    ""
                },
            ),
        };
        let mut checks = String::new();
        if entry.force && !busybox {
            checks += "anvil_r=${anvil_line#*:*:}; anvil_uid=${anvil_r%%:*}; anvil_r=${anvil_r#*:}\n\
                       anvil_r=${anvil_r#*:}; anvil_r=${anvil_r#*:}; anvil_home=${anvil_r%%:*}\n";
            if entry.remove_home {
                checks += &format!(
                    "if [ -e \"$anvil_home\" ]; then\n\
                     anvil_o=$(ls -dn -- \"$anvil_home\" | awk '{{ print $3 }}')\n\
                     [ \"$anvil_o\" = \"$anvil_uid\" ] || {{ echo {}\"$anvil_home\"{} >&2; exit 1; }}\n\
                     fi\n",
                    quote(&format!("{at}: home ")),
                    quote(&format!(" is not owned by {name}; not removed with force")),
                );
            }
            checks += &format!(
                "if anvil_gl=$(anvil_get group {name} 1); then\n\
                 anvil_gg=${{anvil_gl#*:*:}}; anvil_gg=${{anvil_gg%%:*}}\n\
                 anvil_other=$({{ if [ -n \"$anvil_g\" ]; then getent passwd; else cat /etc/passwd; fi; }} \
                 | awk -F: -v g=\"$anvil_gg\" -v n={name} '$4 == g && $1 != n {{ print $1; exit }}')\n\
                 [ -z \"$anvil_other\" ] || {{ echo {}\"$anvil_other\" >&2; exit 1; }}\n\
                 fi\n",
                quote(&format!("{at}: group {name} is the primary group of user ")),
            );
        }
        return format!(
            "{}if [ -z \"$anvil_line\" ]; then {unchanged}else\n\
             {checks}{delete} >&2 || exit\n\
             {}fi\n}}\n",
            read_user(name, sudo),
            line(format!("removed user {name}")),
        );
    }
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
    format!(
        "{read}{checks}\
         if [ -z \"$anvil_line\" ]; then\n{create}{created}else\n\
         anvil_r=${{anvil_line#*:*:}}; anvil_uid=${{anvil_r%%:*}}; anvil_r=${{anvil_r#*:}}\n\
         anvil_gid=${{anvil_r%%:*}}; anvil_r=${{anvil_r#*:}}; anvil_gecos=${{anvil_r%%:*}}\n\
         anvil_r=${{anvil_r#*:}}; anvil_home=${{anvil_r%%:*}}; anvil_shell=${{anvil_r#*:}}\n\
         anvil_fixed=; set --\n\
         {fix}\
         if [ -n \"$anvil_fixed\" ]; then\n{apply}\
         echo {fixed}\"${{anvil_fixed#, }}\"\n\
         else {unchanged}fi\nfi\n}}\n",
        read = read_user(name, sudo),
        fixed = quote(&format!("fixed user {name}: ")),
    )
}

/// The opening of [`script`]: the brace group, `sudo true` when `sudo` is
/// set, the `anvil_get DB KEY FIELD` reader, and user `name`'s passwd line
/// in `anvil_line`, empty when there is none.
fn read_user(name: &str, sudo: &str) -> String {
    let check = if sudo.is_empty() {
        String::new()
    } else {
        format!("{sudo}true || exit\n")
    };
    format!(
        "{{ LC_ALL=C; export LC_ALL; set -f\n\
         {check}\
         anvil_g=; command -v getent >/dev/null 2>&1 && anvil_g=1\n\
         anvil_get() {{ if [ -n \"$anvil_g\" ]; then getent \"$1\" \"$2\"; else \
         awk -F: -v k=\"$2\" -v f=\"$3\" '$f == k {{ print; n = 1; exit }} END {{ exit n ? 0 : 2 }}' \
         \"/etc/$1\"; fi; }}\n\
         anvil_line=$(anvil_get passwd {name} 1); anvil_rc=$?\n\
         case $anvil_rc in 0) ;; 2) anvil_line= ;; *) {unread} ;; esac\n",
        unread = fail(&format!("users: cannot read user {name}")),
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

/// The groups whose loss may lock the login user out of sudo.
const ADMIN_GROUPS: [&str; 3] = ["wheel", "sudo", "admin"];

/// The last line of the [`query`] output.
const QUERY_END: &str = "anvil-users-end=1";

/// The user items of `expanded` that `target` matches, each with its label.
fn matched<'a>(
    expanded: &'a Expanded,
    target: &'a Target,
    facts: &'a Facts,
) -> impl Iterator<Item = (String, &'a UserEntry)> {
    expanded
        .items
        .iter()
        .filter_map(move |item| match &item.kind {
            ItemKind::User { entry, ordinal } if item.matches(target, facts) => {
                Some((item.label("users", *ordinal), entry))
            }
            _ => None,
        })
}

/// Whether the shell locks a login out: its basename is `nologin` or `false`.
fn locks_out(shell: &str) -> bool {
    matches!(shell.rsplit('/').next(), Some("nologin" | "false"))
}

/// The guards the config alone decides, for the user items `target`
/// matches: removing the login user (the probe's `user` fact) or root, or
/// giving the login user a `nologin` or `false` shell. The error is the
/// target's failure.
pub fn check_config(expanded: &Expanded, target: &Target, facts: &Facts) -> Result<(), String> {
    let login = facts.get("user");
    for (label, entry) in matched(expanded, target, facts) {
        let name = entry.name.as_str();
        if entry.absent() && name == "root" {
            return Err(format!("{label} root: refused"));
        }
        if entry.absent() && login == Some(name) {
            return Err(format!("{label} {name}: is the login user"));
        }
        if let Some(shell) = &entry.shell
            && login == Some(name)
            && locks_out(shell)
        {
            return Err(format!(
                "{label} {name}: shell {shell} would lock out the login user"
            ));
        }
    }
    Ok(())
}

/// Where the query reads `/proc`'s mount options.
const MOUNTS: &str = "/proc/mounts";

/// The read-only sh script whose output [`check_state`] reads, or `None`
/// when no user item `target` matches has a guard that needs the target's
/// state: `exclusive_groups` on the login user, or removing a user without
/// `force`. It prints the login user's `id -Gn`, `id -G` and `id -gn`
/// groups (`unknown` when `id` cannot tell) and the gids of `wheel`, `sudo`
/// and `admin`; and per user to remove `busy.NAME=yes|no|unknown`: whether
/// it owns a process, from `pgrep -u`, or the effective uid of
/// `/proc/*/status` without pgrep. It is `unknown` when neither can tell, or
/// when a non-root login cannot see other users' processes (`/proc` mounted
/// with `hidepid`, or a `/proc` whose mount options cannot be read). A user
/// that does not exist is `no`.
pub fn query(expanded: &Expanded, target: &Target, facts: &Facts) -> Option<String> {
    query_with(expanded, target, facts, MOUNTS)
}

/// [`query`], reading `/proc`'s mount options from `mounts`.
fn query_with(expanded: &Expanded, target: &Target, facts: &Facts, mounts: &str) -> Option<String> {
    let login = facts.get("user");
    let mut body = String::new();
    let mut groups = false;
    for (_, entry) in matched(expanded, target, facts) {
        let name = entry.name.as_str();
        if entry.absent() && !entry.force {
            body += &format!("echo \"busy.{name}=$(anvil_busy {name})\"\n");
        } else if !entry.absent() && entry.exclusive_groups && login == Some(name) && !groups {
            groups = true;
            body += &format!(
                "anvil_v=$(id -Gn {name} 2>/dev/null); echo \"login_groups=${{anvil_v:-unknown}}\"\n\
                 anvil_v=$(id -G {name} 2>/dev/null); echo \"login_gids=${{anvil_v:-unknown}}\"\n\
                 echo \"login_primary=$(id -gn {name} 2>/dev/null)\"\n"
            );
            for group in ADMIN_GROUPS {
                body += &format!("echo \"gid.{group}=$(anvil_gid {group})\"\n");
            }
        }
    }
    if body.is_empty() {
        return None;
    }
    Some(format!(
        "{{ LC_ALL=C; export LC_ALL; set -f\n\
         anvil_gid() {{ {{ if command -v getent >/dev/null 2>&1; then getent group \"$1\"; \
         else awk -F: -v k=\"$1\" '$1 == k' /etc/group; fi; }} 2>/dev/null | cut -d: -f3; }}\n\
         anvil_hidden() {{ [ \"$(id -u)\" != 0 ] || return 1\n\
         awk '$2 == \"/proc\" {{ n = split($4, o, \",\"); \
         for (i = 1; i <= n; i++) if (o[i] ~ /^hidepid=/ && o[i] != \"hidepid=0\" && o[i] != \"hidepid=off\") h = 1 }} \
         END {{ exit h ? 0 : 1 }}' {mounts} 2>/dev/null\n\
         case $? in 0) return 0 ;; 1) return 1 ;; esac\n\
         [ -d /proc/self ]; }}\n\
         anvil_busy() {{ id -u \"$1\" >/dev/null 2>&1 || {{ echo no; return; }}\n\
         if anvil_hidden; then echo unknown\n\
         elif command -v pgrep >/dev/null 2>&1; then pgrep -u \"$1\" >/dev/null 2>&1\n\
         case $? in 0) echo yes ;; 1) echo no ;; *) echo unknown ;; esac\n\
         elif [ -r /proc/self/status ]; then anvil_u=$(id -u \"$1\"); set +f\n\
         anvil_r=$(awk -v u=\"$anvil_u\" '/^Uid:/ && $3 == u {{ f = 1; exit }} \
         END {{ print f ? \"yes\" : \"no\" }}' /proc/[0-9]*/status 2>/dev/null); set -f\n\
         echo \"${{anvil_r:-unknown}}\"\n\
         else echo unknown; fi; }}\n\
         {body}echo {QUERY_END}\n}} </dev/null\n",
        mounts = quote(mounts),
    ))
}

/// The guards [`query`]'s `output` decides, as the target was before the
/// run: `exclusive_groups` dropping the login user from `wheel`, `sudo` or
/// `admin` while it is a member, and removing a user that owns a process
/// without `force`. The error is the target's failure.
pub fn check_state(
    expanded: &Expanded,
    target: &Target,
    facts: &Facts,
    output: &[u8],
) -> Result<(), String> {
    let text = String::from_utf8_lossy(output);
    let state: BTreeMap<&str, &str> = text
        .lines()
        .take_while(|line| *line != QUERY_END)
        .filter_map(|line| line.split_once('='))
        .collect();
    if !text.lines().any(|line| line == QUERY_END) {
        return Err("users: the account query output has no end marker".to_owned());
    }
    let login = facts.get("user");
    for (label, entry) in matched(expanded, target, facts) {
        let name = entry.name.as_str();
        if entry.absent() && !entry.force {
            match state.get(format!("busy.{name}").as_str()) {
                Some(&"no") => {}
                Some(&"yes") => return Err(format!("{label} {name}: owns running processes")),
                _ => {
                    return Err(format!(
                        "{label} {name}: cannot tell whether it owns running processes; \
                         set force = true to remove it anyway"
                    ));
                }
            }
        }
        if let Some(groups) = &entry.groups
            && entry.exclusive_groups
            && !entry.absent()
            && login == Some(name)
        {
            let known = |key: &str| state.get(key).copied().filter(|value| *value != "unknown");
            let (Some(names), Some(gids)) = (known("login_groups"), known("login_gids")) else {
                return Err(format!(
                    "{label} {name}: cannot tell whether the login user is in wheel, sudo or admin"
                ));
            };
            let gid = |group: &str| known(&format!("gid.{group}")).filter(|gid| !gid.is_empty());
            let member = |group: &str| {
                names.split_whitespace().any(|have| have == group)
                    || gid(group).is_some_and(|gid| gids.split_whitespace().any(|have| have == gid))
            };
            let primary = |group: &str| match entry.group.as_deref() {
                Some(set) if set.bytes().all(|b| b.is_ascii_digit()) => gid(group) == Some(set),
                Some(set) => set == group,
                None => state.get("login_primary") == Some(&group),
            };
            let dropped = ADMIN_GROUPS.into_iter().find(|group| {
                member(group) && !primary(group) && !groups.iter().any(|kept| kept == group)
            });
            if let Some(group) = dropped {
                return Err(format!(
                    "{label} {name}: would drop the login user from {group}"
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::config::Config;
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
                "#!/bin/sh\n[ $# -eq 1 ] && exec cat '{d}'/\"$1\"\n\
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
            for tool in [
                "useradd", "usermod", "userdel", "adduser", "deluser", "addgroup", "delgroup",
            ] {
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

    #[test]
    fn an_absent_user_is_removed_once_and_a_gone_one_is_unchanged() {
        for (accounts, keys, wanted) in [
            (Accounts::Shadow, "", "userdel deploy\n"),
            (
                Accounts::Shadow,
                "remove_home = true\nforce = true",
                "userdel -f -r deploy\n",
            ),
            (Accounts::Busybox, "", "deluser deploy\n"),
            (
                Accounts::Busybox,
                "remove_home = true",
                "deluser --remove-home deploy\n",
            ),
        ] {
            let absent = entry(&format!("state = \"absent\"\n{keys}"));
            let (code, out, err, log) = Fake::new(PASSWD).run(&absent, accounts);
            assert_eq!(
                (code, out.as_str(), log.as_str()),
                (Some(0), "removed user deploy\n", wanted),
                "{err}"
            );
            let (code, out, _, log) =
                Fake::new("root:x:0:0::/root:/bin/sh\n").run(&absent, accounts);
            assert_eq!(
                (code, out.as_str(), log.as_str()),
                (Some(0), "unchanged user deploy\n", "")
            );
        }
        assert_eq!(shown(&entry("state = \"absent\"")), "remove user deploy");
    }

    /// Facts of a target the non-root `deploy` logs in to.
    fn login(user: &str) -> Facts {
        Facts(BTreeMap::from([("user".to_owned(), user.to_owned())]))
    }

    fn users(text: &str) -> Expanded {
        crate::tasks::legacy(&toml::from_str::<Config>(text).unwrap())
    }

    #[test]
    fn config_guards_refuse_removing_the_login_user_or_root_and_a_locking_shell() {
        let target = crate::test_util::target("web");
        let facts = login("deploy");
        for (text, wanted) in [
            (
                "[[users]]\nname = \"web\"\n[[users]]\nname = \"deploy\"\nstate = \"absent\"\nforce = true\n",
                "users[1] deploy: is the login user",
            ),
            (
                "[[users]]\nname = \"root\"\nstate = \"absent\"\n",
                "users[0] root: refused",
            ),
            (
                "[[users]]\nname = \"deploy\"\nshell = \"/usr/sbin/nologin\"\n",
                "users[0] deploy: shell /usr/sbin/nologin would lock out the login user",
            ),
            (
                "[[users]]\nname = \"deploy\"\nshell = \"/bin/false\"\n",
                "users[0] deploy: shell /bin/false would lock out the login user",
            ),
        ] {
            assert_eq!(
                check_config(&users(text), &target, &facts),
                Err(wanted.to_owned()),
                "{text}"
            );
        }
        let task = Expanded {
            items: vec![crate::tasks::Item {
                path: Some("tasks[1]".to_owned()),
                name: None,
                tags: Vec::new(),
                facts: Vec::new(),
                notify: Vec::new(),
                kind: ItemKind::User {
                    entry: entry("state = \"absent\""),
                    ordinal: 0,
                },
            }],
            from_tasks: true,
        };
        assert_eq!(
            check_config(&task, &target, &facts),
            Err("tasks[1].user deploy: is the login user".to_owned())
        );
        for text in [
            "[[users]]\nname = \"web\"\nshell = \"/usr/sbin/nologin\"\n",
            "[[users]]\nname = \"web\"\nstate = \"absent\"\n",
            "[[users]]\nname = \"deploy\"\nshell = \"/bin/bash\"\n",
            "[[users]]\nname = \"deploy\"\nstate = \"absent\"\ntags = [\"db\"]\n",
        ] {
            assert_eq!(
                check_config(&users(text), &target, &facts),
                Ok(()),
                "{text}"
            );
        }
    }

    #[test]
    fn the_query_runs_only_for_a_state_guard() {
        let target = crate::test_util::target("web");
        let facts = login("deploy");
        for text in [
            "[[users]]\nname = \"deploy\"\ngroups = [\"wheel\"]\n",
            "[[users]]\nname = \"web\"\ngroups = [\"a\"]\nexclusive_groups = true\n",
            "[[users]]\nname = \"old\"\nstate = \"absent\"\nforce = true\n",
            "[[users]]\nname = \"old\"\nstate = \"absent\"\ntags = [\"db\"]\n",
        ] {
            assert_eq!(query(&users(text), &target, &facts), None, "{text}");
        }
        let script = query(
            &users(
                "[[users]]\nname = \"deploy\"\ngroups = [\"a\"]\nexclusive_groups = true\n\
                 [[users]]\nname = \"old\"\nstate = \"absent\"\n",
            ),
            &target,
            &facts,
        )
        .unwrap();
        for wanted in [
            "anvil_v=$(id -Gn deploy 2>/dev/null); echo \"login_groups=${anvil_v:-unknown}\"\n",
            "anvil_v=$(id -G deploy 2>/dev/null); echo \"login_gids=${anvil_v:-unknown}\"\n",
            "echo \"gid.wheel=$(anvil_gid wheel)\"\n",
            "echo \"busy.old=$(anvil_busy old)\"\n",
            "echo anvil-users-end=1\n",
        ] {
            assert!(script.contains(wanted), "{wanted}\n---\n{script}");
        }
        assert!(
            !script.contains("sudo -n") && !script.contains("sudo true"),
            "{script}"
        );
    }

    #[test]
    fn state_guards_refuse_dropping_the_login_user_from_an_admin_group_or_removing_a_busy_user() {
        let target = crate::test_util::target("web");
        let facts = login("deploy");
        let state = |text: &str, output: &str| {
            check_state(&users(text), &target, &facts, output.as_bytes())
        };
        let exclusive =
            "[[users]]\nname = \"deploy\"\ngroups = [\"docker\"]\nexclusive_groups = true\n";
        let member = "login_groups=deploy docker wheel\nlogin_gids=1500 2001 10\nlogin_primary=deploy\nanvil-users-end=1\n";
        assert_eq!(
            state(exclusive, member),
            Err("users[0] deploy: would drop the login user from wheel".to_owned())
        );
        for (text, output) in [
            (
                exclusive,
                "login_groups=deploy docker\nlogin_gids=1500 2001 10\nlogin_primary=deploy\nanvil-users-end=1\n",
            ),
            (
                "[[users]]\nname = \"deploy\"\ngroups = [\"docker\", \"wheel\"]\nexclusive_groups = true\n",
                member,
            ),
            (
                exclusive,
                "login_groups=wheel docker\nlogin_gids=1500 2001 10\nlogin_primary=wheel\nanvil-users-end=1\n",
            ),
            (
                "[[users]]\nname = \"web\"\ngroups = []\nexclusive_groups = true\n",
                member,
            ),
        ] {
            assert_eq!(state(text, output), Ok(()), "{text}{output}");
        }
        assert_eq!(
            state(
                "[[users]]\nname = \"deploy\"\ngroup = \"docker\"\ngroups = []\nexclusive_groups = true\n",
                "login_groups=sudo docker\nlogin_gids=1500 2001 10\nlogin_primary=sudo\nanvil-users-end=1\n",
            ),
            Err("users[0] deploy: would drop the login user from sudo".to_owned())
        );
        for output in [
            "login_primary=deploy\nanvil-users-end=1\n",
            "login_groups=unknown\nlogin_gids=unknown\nlogin_primary=\nanvil-users-end=1\n",
            "login_groups=deploy wheel\nlogin_gids=unknown\nanvil-users-end=1\n",
        ] {
            assert_eq!(
                state(exclusive, output),
                Err(
                    "users[0] deploy: cannot tell whether the login user is in wheel, sudo or admin"
                        .to_owned()
                ),
                "{output}"
            );
        }
        let numeric =
            "[[users]]\nname = \"deploy\"\ngroup = \"10\"\ngroups = []\nexclusive_groups = true\n";
        let by_gid = |primary: &str| {
            format!(
                "login_groups=deploy wheel\nlogin_gids=1500 10\nlogin_primary={primary}\n\
                 gid.wheel=10\ngid.sudo=\ngid.admin=\nanvil-users-end=1\n"
            )
        };
        assert_eq!(state(numeric, &by_gid("deploy")), Ok(()), "gid 10 is wheel");
        assert_eq!(
            state(&numeric.replace("\"10\"", "\"1500\""), &by_gid("wheel")),
            Err("users[0] deploy: would drop the login user from wheel".to_owned())
        );
        assert_eq!(
            state(
                exclusive,
                "login_groups=deploy\nlogin_gids=1500 27\nlogin_primary=deploy\n\
                 gid.wheel=\ngid.sudo=27\ngid.admin=\nanvil-users-end=1\n"
            ),
            Err("users[0] deploy: would drop the login user from sudo".to_owned()),
            "a membership by gid counts"
        );
        let absent = "[[users]]\nname = \"old\"\nstate = \"absent\"\n";
        assert_eq!(
            state(absent, "busy.old=yes\nanvil-users-end=1\n"),
            Err("users[0] old: owns running processes".to_owned())
        );
        assert_eq!(state(absent, "busy.old=no\nanvil-users-end=1\n"), Ok(()));
        assert_eq!(
            state(absent, "busy.old=unknown\nanvil-users-end=1\n"),
            Err(
                "users[0] old: cannot tell whether it owns running processes; \
                 set force = true to remove it anyway"
                    .to_owned()
            )
        );
        assert_eq!(
            state(&format!("{absent}force = true\n"), "anvil-users-end=1\n"),
            Ok(())
        );
        assert_eq!(
            state(absent, "busy.old=no\n"),
            Err("users: the account query output has no end marker".to_owned())
        );
    }

    fn whoami() -> String {
        let out = Local.run("id -un", RUN_TIMEOUT).unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    /// The query for removing the user running the tests, who owns this
    /// process, and one that does not exist, under `path`.
    fn busy(path: &str) -> String {
        busy_with(path, MOUNTS)
    }

    /// [`busy`], reading `/proc`'s mount options from `mounts`.
    fn busy_with(path: &str, mounts: &str) -> String {
        let me = whoami();
        let text = format!(
            "[[users]]\nname = \"{me}\"\nstate = \"absent\"\n\
             [[users]]\nname = \"anvil_nobody_x\"\nstate = \"absent\"\n"
        );
        let script = query_with(
            &users(&text),
            &crate::test_util::target("web"),
            &login("root"),
            mounts,
        )
        .unwrap();
        let out = Local
            .run(&format!("PATH={path}\n{script}"), RUN_TIMEOUT)
            .unwrap();
        assert_eq!(out.code, Some(0));
        String::from_utf8(out.stdout).unwrap().replace(&me, "ME")
    }

    #[test]
    fn the_query_finds_a_users_processes_with_pgrep() {
        let path = std::env::var("PATH").unwrap();
        assert_eq!(
            busy(&path),
            "busy.ME=yes\nbusy.anvil_nobody_x=no\nanvil-users-end=1\n"
        );
    }

    /// Without pgrep, `/proc` answers on Linux; with neither, `unknown`.
    #[test]
    fn the_query_without_pgrep_reads_proc_or_says_unknown() {
        let dir = temp_dir("users-query");
        for tool in ["id", "awk"] {
            let real = Local
                .run(&format!("command -v {tool}"), RUN_TIMEOUT)
                .unwrap();
            let real = String::from_utf8(real.stdout).unwrap();
            std::os::unix::fs::symlink(real.trim(), dir.join(tool)).unwrap();
        }
        let wanted = if cfg!(target_os = "linux") {
            "yes"
        } else {
            "unknown"
        };
        assert_eq!(
            busy(dir.to_str().unwrap()),
            format!("busy.ME={wanted}\nbusy.anvil_nobody_x=no\nanvil-users-end=1\n")
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A non-root login that cannot see other users' processes cannot tell.
    #[test]
    fn the_query_says_unknown_under_hidepid_for_a_non_root_login() {
        let dir = temp_dir("users-mounts");
        let mounts = dir.join("mounts");
        let path = std::env::var("PATH").unwrap();
        let root = whoami() == "root";
        for (options, hidden) in [
            ("rw,nosuid,hidepid=2", true),
            ("rw,hidepid=invisible,gid=10", true),
            ("rw,hidepid=1", true),
            ("rw,hidepid=0", false),
            ("rw,hidepid=off", false),
            ("rw,nosuid", false),
        ] {
            fs::write(
                &mounts,
                format!("sysfs /sys sysfs rw 0 0\nproc /proc proc {options} 0 0\n"),
            )
            .unwrap();
            let wanted = if hidden && !root { "unknown" } else { "yes" };
            assert_eq!(
                busy_with(&path, mounts.to_str().unwrap()),
                format!("busy.ME={wanted}\nbusy.anvil_nobody_x=no\nanvil-users-end=1\n"),
                "{options}"
            );
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    fn my_uid() -> String {
        let out = Local.run("id -u", RUN_TIMEOUT).unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    #[test]
    fn force_with_remove_home_removes_only_a_home_the_user_owns() {
        let keys = "state = \"absent\"\nremove_home = true\nforce = true";
        let home = temp_dir("users-home");
        let me = my_uid();
        let passwd = |uid: &str| {
            format!(
                "root:x:0:0::/root:/bin/sh\ndeploy:x:{uid}:100:Deploy:{}:/bin/sh\n",
                home.display()
            )
        };
        let (code, out, err, log) = Fake::new(&passwd("1500")).run(&entry(keys), Accounts::Shadow);
        assert_eq!((code, out.as_str(), log.as_str()), (Some(1), "", ""));
        assert_eq!(
            err,
            format!(
                "users[0] deploy: home {} is not owned by deploy; not removed with force\n",
                home.display()
            )
        );
        let (code, out, err, log) = Fake::new(&passwd(&me)).run(&entry(keys), Accounts::Shadow);
        assert_eq!(
            (code, out.as_str(), log.as_str()),
            (Some(0), "removed user deploy\n", "userdel -f -r deploy\n"),
            "{err}"
        );
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn force_refuses_to_leave_another_users_primary_group_dangling() {
        let keys = "state = \"absent\"\nforce = true";
        let fake = Fake::new(&format!("{PASSWD}web:x:1501:1600::/home/web:/bin/sh\n"));
        fs::write(fake.dir.join("group"), format!("{GROUP}deploy:x:1600:\n")).unwrap();
        let (code, out, err, log) = fake.run(&entry(keys), Accounts::Shadow);
        assert_eq!((code, out.as_str(), log.as_str()), (Some(1), "", ""));
        assert_eq!(
            err,
            "users[0] deploy: group deploy is the primary group of user web\n"
        );
        let fake = Fake::new(&format!("{PASSWD}web:x:1501:100::/home/web:/bin/sh\n"));
        fs::write(fake.dir.join("group"), format!("{GROUP}deploy:x:1600:\n")).unwrap();
        let (code, out, _, log) = fake.run(&entry(keys), Accounts::Shadow);
        assert_eq!(
            (code, out.as_str(), log.as_str()),
            (Some(0), "removed user deploy\n", "userdel -f deploy\n")
        );
    }

    /// Unreadable mount options of a `/proc` that exists count as hidden; a
    /// target without `/proc` has no hidepid.
    #[test]
    fn the_query_says_unknown_when_a_non_root_login_cannot_read_the_mounts() {
        let dir = temp_dir("users-mounts");
        let unreadable = dir.join("mounts");
        fs::write(&unreadable, "proc /proc proc rw 0 0\n").unwrap();
        chmod(&unreadable, 0o000);
        let path = std::env::var("PATH").unwrap();
        let hidden = whoami() != "root" && Path::new("/proc/self").is_dir();
        let wanted = if hidden { "unknown" } else { "yes" };
        for mounts in [unreadable.clone(), dir.join("missing")] {
            assert_eq!(
                busy_with(&path, mounts.to_str().unwrap()),
                format!("busy.ME={wanted}\nbusy.anvil_nobody_x=no\nanvil-users-end=1\n"),
                "{}",
                mounts.display()
            );
        }
        fs::remove_dir_all(&dir).unwrap();
    }
}
