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
//! `removed user NAME`, or `unchanged user NAME` when it is gone. Nothing of
//! the account is recorded for rollback.
//!
//! Before any step on a target, the lockout guards fail it: [`check_config`]
//! from the config and the probe's `user` fact, then [`check_state`] from
//! the read-only [`query`], as the target was before the run.
//!
//! An entry with `authorized_keys` gets a file step right after its own
//! ([`keys_run`]): `HOME/.ssh/authorized_keys`, HOME as the query found it
//! or as the user step sets it, written 0600 as the user (`su` from a root
//! login, `sudo -n -u` from another), so the walk checks every directory
//! for root or that user, as a `sudo = false` `[[files]]` write does, and
//! creates a missing `.ssh` 0700. The capture reads the file whatever its
//! mode; the keys are added to it, or with `exclusive_keys` replace it
//! ([`KeysWrite::merged`]), and the record keeps its previous text, which
//! rollback writes back as the same user.

use std::collections::BTreeMap;
use std::path::PathBuf;

use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::bytes::{hex, unbase64};
use crate::config::{Config, UserEntry, is_account_name};
use crate::error::Failure;
use crate::facts::Facts;
use crate::files::{
    Captured, FileWrite, Rendered, Sources, skip_run, store_reason, sudo_secret_for,
};
use crate::filestore::{self, StoreError};
use crate::groups::Accounts;
use crate::inventory::Target;
use crate::remote::{Walk, WalkNonce, login_checks, login_top, quote, refusal, zeroizing};
use crate::step::{FileRun, PlannedRun, RunKind, UserRun};
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
/// group G, groups A B, shell PATH, home PATH, system, no home,
/// authorized_keys N)` for those set, the last with `, exclusive` under
/// `exclusive_keys`; `only groups` when `exclusive_groups` is, `no groups`
/// for an empty exclusive list, and nothing for an empty list that changes
/// nothing; or `remove user NAME`.
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
        entry.authorized_keys.as_ref().map(|keys| {
            let exclusive = if entry.exclusive_keys {
                ", exclusive"
            } else {
                ""
            };
            format!("authorized_keys {}{exclusive}", keys.len())
        }),
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
        let mut checks = format!(
            "anvil_r=${{anvil_line#*:*:}}; anvil_uid=${{anvil_r%%:*}}; anvil_r=${{anvil_r#*:}}\n\
             anvil_r=${{anvil_r#*:}}; anvil_r=${{anvil_r#*:}}; anvil_home=${{anvil_r%%:*}}\n\
             case $anvil_uid in ''|*[!0]*) ;; *) {} ;; esac\n",
            fail(&format!("{at}: has uid 0; refused")),
        );
        if entry.remove_home {
            checks += &format!(
                "case $anvil_home in ''|*[!/]*) ;; *) {} ;; esac\n\
                 if [ -e \"$anvil_home\" ]; then\n\
                 anvil_o=$(ls -dn -- \"$anvil_home\" | awk '{{ print $3 }}')\n\
                 [ \"$anvil_o\" = \"$anvil_uid\" ] || {{ echo {}\"$anvil_home\"{} >&2; exit 1; }}\n\
                 fi\n",
                fail(&format!("{at}: home / is not removed")),
                quote(&format!("{at}: home ")),
                quote(&format!(" is not owned by {name}; not removed")),
            );
        }
        if entry.force && !busybox {
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

/// The key types an `authorized_keys` line may have; certificates are left out.
const KEY_TYPES: [&str; 8] = [
    "ssh-ed25519",
    "ssh-rsa",
    "ssh-dss",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
    "sk-ssh-ed25519@openssh.com",
    "sk-ecdsa-sha2-nistp256@openssh.com",
];

/// One OpenSSH public key: its type and base64 blob, which is what tells two
/// keys apart, whatever their options and comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    kind: String,
    blob: String,
}

/// The first field of `line` and the rest after it: up to the first blank
/// outside double quotes, where `\"` stays inside them.
fn field(line: &str) -> (&str, &str) {
    let line = line.trim_start_matches([' ', '\t']);
    let (mut quoted, mut escaped) = (false, false);
    for (at, c) in line.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ' ' | '\t' if !quoted => return (&line[..at], &line[at..]),
            _ => {}
        }
    }
    (line, "")
}

/// `line` as one OpenSSH public key line: `[options] TYPE BASE64 [comment]`,
/// TYPE one of [`KEY_TYPES`] and BASE64 a blob that names the same type.
pub(crate) fn parse_key(line: &str) -> Result<Key, String> {
    if line.contains(['\n', '\r']) {
        return Err("must be one line".to_owned());
    }
    if line.trim_start_matches([' ', '\t']).starts_with('#') {
        return Err("is a comment, not a key".to_owned());
    }
    let (first, rest) = field(line);
    let (kind, rest) = if KEY_TYPES.contains(&first) {
        (first, rest)
    } else {
        field(rest)
    };
    if !KEY_TYPES.contains(&kind) {
        return Err("is not an OpenSSH public key line".to_owned());
    }
    let (blob, _) = field(rest);
    let decoded = unbase64(blob).ok_or("has no base64 key after its type")?;
    let named = decoded
        .get(..4)
        .map(|len| u32::from_be_bytes([len[0], len[1], len[2], len[3]]) as usize)
        .and_then(|len| decoded.get(4..4 + len));
    if named != Some(kind.as_bytes()) {
        return Err(format!("has a key that is not {kind}"));
    }
    Ok(Key {
        kind: kind.to_owned(),
        blob: blob.to_owned(),
    })
}

/// The keys of an authorized_keys `text`, `#` and blank lines skipped;
/// `None` when any other line is not a key line ([`parse_key`]), a CRLF
/// one included.
pub(crate) fn file_keys(text: &str) -> Option<Vec<Key>> {
    text.split('\n')
        .filter(|line| {
            let line = line.trim_matches([' ', '\t']);
            !line.is_empty() && !line.starts_with('#')
        })
        .map(|line| parse_key(line).ok())
        .collect()
}

/// A key line as an error names it: its first 40 characters.
pub(crate) fn shown_key(line: &str) -> String {
    match line.char_indices().nth(40) {
        Some((at, _)) => format!("{}...", &line[..at]),
        None => line.to_owned(),
    }
}

/// The key lines `authorized_keys` reads from the store, by user ordinal and
/// index in the list.
#[derive(Debug, Default)]
pub struct StoreKeys(BTreeMap<(usize, usize), String>);

/// Reads each `$/` item of each user item's `authorized_keys` from the
/// store, as `[[files]]` reads a store src; each must hold one key line
/// ([`parse_key`]), a final newline allowed. The errors name the item.
/// The workspace is resolved only when there is one.
pub fn load_keys(
    expanded: &Expanded,
    workspace: impl FnOnce() -> Result<PathBuf, Failure>,
) -> Result<StoreKeys, Failure> {
    let stored: Vec<(&Item, usize, usize, &str)> = expanded
        .items
        .iter()
        .filter_map(|item| match &item.kind {
            ItemKind::User { entry, ordinal } => Some((item, *ordinal, entry)),
            _ => None,
        })
        .flat_map(|(item, i, entry)| {
            (entry.authorized_keys.iter().flatten().enumerate())
                .filter(|(_, key)| filestore::is_store_path(key))
                .map(move |(j, key)| (item, i, j, key.as_str()))
        })
        .collect();
    if stored.is_empty() {
        return Ok(StoreKeys::default());
    }
    let store = filestore::Root::in_workspace(&workspace()?);
    let mut keys = BTreeMap::new();
    for (item, i, j, src) in stored {
        let named = |why: &str| {
            item.shown_message(format!(
                "{}.authorized_keys[{j}] `{src}`: {why}",
                item.label("users", i)
            ))
        };
        let bytes = store.read_trusted_bytes(src).map_err(|err| Failure {
            refused: matches!(err, StoreError::Untrusted(_, _)),
            ..Failure::usage(named(&store_reason(src, &err)))
        })?;
        let text = String::from_utf8(bytes).map_err(|_| Failure::usage(named("is not text")))?;
        let line = text.strip_suffix('\n').unwrap_or(&text);
        let line = line.strip_suffix('\r').unwrap_or(line);
        parse_key(line).map_err(|why| Failure::usage(named(&why)))?;
        keys.insert((i, j), line.to_owned());
    }
    Ok(StoreKeys(keys))
}

/// The key lines of user `entry`, the `i`th user item: each inline line as
/// written, each `$/` one as [`load_keys`] read it.
fn listed(entry: &UserEntry, i: usize, store: &StoreKeys) -> Result<Vec<String>, String> {
    (entry.authorized_keys.iter().flatten().enumerate())
        .map(|(j, key)| {
            if filestore::is_store_path(key) {
                (store.0.get(&(i, j)).cloned())
                    .ok_or_else(|| format!("users[{i}].authorized_keys[{j}]: was not loaded"))
            } else {
                Ok(key.clone())
            }
        })
        .collect()
}

/// The home of each user item with `authorized_keys` a target matches, by
/// ordinal: its `home`, else the one the account query found, else for a
/// user that does not exist yet `/home/NAME`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Homes(BTreeMap<usize, String>);

/// How an authorized_keys write of one user reaches the target: the keys,
/// whether they replace the file, and the command that runs the write's
/// scripts as that user, `""` when it is the login user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeysWrite {
    /// `users[N] NAME`, which starts its failures.
    pub label: String,
    pub user: String,
    pub keys: Vec<String>,
    pub exclusive: bool,
    pub wrap: String,
}

impl KeysWrite {
    /// `script` run through [`Self::wrap`] on its stdin, from `/`.
    pub(crate) fn wrapped(&self, script: &str) -> Zeroizing<String> {
        wrapped(&self.wrap, script)
    }

    /// The file `captured` becomes: the listed keys added to it, each not
    /// there yet at the end, or with `exclusive` only the listed keys, one
    /// per line. A file the capture could not read fails, naming `dest`.
    pub(crate) fn merged(&self, dest: &str, captured: &Captured) -> Result<String, String> {
        let current = match captured {
            Captured::Absent => "",
            Captured::File {
                bytes: Some(bytes), ..
            } => std::str::from_utf8(bytes)
                .map_err(|_| format!("{}: {dest} is not text", self.label))?,
            Captured::File { reason, .. } => {
                return Err(format!(
                    "{}: cannot read {dest} ({})",
                    self.label,
                    reason.unwrap_or("unreadable")
                ));
            }
        };
        let mut have: Vec<Key> = if self.exclusive {
            Vec::new()
        } else {
            current
                .lines()
                .filter_map(|line| parse_key(line).ok())
                .collect()
        };
        let mut text = if self.exclusive {
            String::new()
        } else {
            current.to_owned()
        };
        for line in &self.keys {
            let key = parse_key(line).map_err(|why| format!("{}: {why}", self.label))?;
            if have.contains(&key) {
                continue;
            }
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(line);
            text.push('\n');
            have.push(key);
        }
        Ok(text)
    }
}

/// `script` on the stdin of `wrap`, from `/`, or as it is when `wrap` is
/// `""`; the heredoc's delimiter holds the script's own hash.
pub(crate) fn wrapped(wrap: &str, script: &str) -> Zeroizing<String> {
    if wrap.is_empty() {
        return Zeroizing::new(script.to_owned());
    }
    let end = format!(
        "ANVIL_AS_{}",
        &hex(&Sha256::digest(script.as_bytes()))[..32]
    );
    zeroizing(format_args!(
        "{wrap} <<'{end}'\ncd / || exit 1\n{script}\n{end}\n"
    ))
}

/// The command that runs a script on its stdin as `user` on a target whose
/// login has `facts`: none for the login user itself, `su` from root, else
/// `sudo -n -u`, which needs the entry's `sudo` and no password; `label`
/// names the entry in the refusal.
pub(crate) fn as_user(
    label: &str,
    user: &str,
    facts: &Facts,
    sudo: bool,
) -> Result<String, String> {
    if facts.get("user") == Some(user) {
        Ok(String::new())
    } else if facts.is_root() {
        Ok(format!("su -s /bin/sh {user}"))
    } else if !sudo {
        Err(format!(
            "{label}: authorized_keys of another user needs sudo"
        ))
    } else if facts.sudo_asks_password() {
        Err(format!(
            "{label}: authorized_keys of another user needs sudo without a password"
        ))
    } else {
        Ok(format!("sudo -n -u {user} /bin/sh"))
    }
}

/// The step after user `item`'s that writes its `authorized_keys`, or `None`
/// when it has none, removes the user, does not match, has no account tools
/// (its user step fails) or `homes` is `None` (plan). It is a file step of
/// `HOME/.ssh/authorized_keys` whose bytes [`KeysWrite::merged`] builds
/// once the capture read the file; until then its script only fails.
pub fn keys_run(
    item: &Item,
    (entry, i): (&UserEntry, usize),
    target: &Target,
    facts: Option<&Facts>,
    (homes, sources): (Option<&Homes>, &Sources),
) -> Result<Option<PlannedRun>, String> {
    let (Some(_), Some(facts), Some(homes)) = (&entry.authorized_keys, facts, homes) else {
        return Ok(None);
    };
    if entry.absent() || !item.matches(target, facts) || Accounts::of(facts).is_none() {
        return Ok(None);
    }
    let name = entry.name.as_str();
    let label = format!("{} {name}", item.label("users", i));
    let home =
        (homes.0.get(&i)).ok_or_else(|| format!("{label}: the account query has no home"))?;
    let dest = format!("{}/.ssh/authorized_keys", home.trim_end_matches('/'));
    let wrap = as_user(&label, name, facts, entry.sudo)?;
    let top = login_top();
    let h = quote(home);
    let missing = refusal(
        "",
        home,
        "",
        &format!("{label}: home {home} does not exist"),
    );
    let checks = format!(
        "{{ test -d {h} || {missing}; }} && {}",
        login_checks(&dest, &top)
    );
    let mut walk = Walk::new("files", &dest, false, &top, Some(WalkNonce::generate()?));
    walk.private = true;
    let keys = listed(entry, i, sources.keys())?;
    log::info!("{}: {label} {dest}, {} keys", target.name, keys.len());
    let write = FileWrite {
        ordinal: i,
        dest: dest.clone(),
        mode: "600".to_owned(),
        sudo: String::new(),
        checks,
        walk: Box::new(walk),
        as_root: false,
        password: false,
        rendered: None,
        ids: None,
        mark: WalkNonce::attrs()?,
        parents: true,
        binary: false,
        item: None,
        secret: None,
        keys: Some(Box::new(KeysWrite {
            label: label.clone(),
            user: name.to_owned(),
            keys,
            exclusive: entry.exclusive_keys,
            wrap,
        })),
    };
    let mut run = skip_run(&dest, format!("file {dest}"));
    run.run = Zeroizing::new(format!(
        "{{ echo {} >&2; exit 1; }}\n",
        quote(&format!(
            "{label}: authorized_keys is written by apply only"
        ))
    ));
    if let RunKind::File { run: file, .. } = &mut run.kind {
        *file = FileRun {
            upload: None,
            skip: false,
            write: Some(write),
        };
    }
    Ok(Some(run))
}

/// `text` as a templated write's rendered bytes, with their hash.
pub(crate) fn rendered(text: String) -> Box<Rendered> {
    let sha256 = hex(&Sha256::digest(text.as_bytes()));
    Box::new(Rendered {
        text: Zeroizing::new(text),
        sha256,
        scrub: Vec::new(),
    })
}

/// The groups whose loss may lock the login user out of sudo.
const ADMIN_GROUPS: [&str; 3] = ["wheel", "sudo", "admin"];

/// Whether `entry` may drop its user from an admin group: `exclusive_groups`
/// drops supplementary groups, and `group` moves the primary one.
fn admin_guard(entry: &UserEntry) -> bool {
    entry.exclusive_groups || entry.group.is_some()
}

/// Defines `anvil_pw NAME`, which prints NAME's passwd line, with getent or
/// awk over `/etc/passwd`, and returns 2 when there is none.
const ANVIL_PW: &str = "anvil_pw() { if command -v getent >/dev/null 2>&1; then getent passwd \"$1\"; \
else awk -F: -v k=\"$1\" '$1 == k { print; f = 1 } END { exit f ? 0 : 2 }' /etc/passwd; fi; }\n";

/// The `[[users]]` entry with `authorized_keys` named `user` that may apply
/// to `target`, as rollback finds the one a keys record was written for.
pub(crate) fn keys_entry<'a>(
    expanded: &'a Expanded,
    target: &Target,
    facts: Option<&Facts>,
    user: &str,
) -> Option<&'a UserEntry> {
    expanded.items.iter().find_map(|item| match &item.kind {
        ItemKind::User { entry, .. }
            if entry.name == user && has_keys(entry) && item.may_match(target, facts) =>
        {
            Some(entry)
        }
        _ => None,
    })
}

/// The read-only script that prints `home.NAME=HOME` for each of `names`
/// with a passwd entry, then the end marker, as the account query does.
pub(crate) fn home_query(names: &[&str]) -> String {
    let body: String = names
        .iter()
        .map(|name| {
            debug_assert!(is_account_name(name), "{name}");
            format!(
                "anvil_l=$(anvil_pw {name}) && echo \"home.{name}=$(printf '%s\\n' \"$anvil_l\" | cut -d: -f6)\"\n"
            )
        })
        .collect();
    format!("{{ LC_ALL=C; export LC_ALL; set -f\n{ANVIL_PW}{body}echo {QUERY_END}\n}} </dev/null\n")
}

/// The homes [`home_query`]'s `output` names, by user; `None` without its
/// end marker.
pub(crate) fn parse_homes(output: &[u8]) -> Option<BTreeMap<String, String>> {
    let text = String::from_utf8_lossy(output);
    let mut homes = BTreeMap::new();
    for line in text.lines() {
        if line == QUERY_END {
            return Some(homes);
        }
        if let Some((name, home)) = line.strip_prefix("home.").and_then(|l| l.split_once('=')) {
            homes.insert(name.to_owned(), home.to_owned());
        }
    }
    None
}

/// The last line of the [`query`] output.
const QUERY_END: &str = "anvil-users-end=1";

/// The user items of `expanded` that `target` matches, each with its label
/// and ordinal.
fn matched<'a>(
    expanded: &'a Expanded,
    target: &'a Target,
    facts: &'a Facts,
) -> impl Iterator<Item = (String, &'a UserEntry, usize)> {
    expanded
        .items
        .iter()
        .filter_map(move |item| match &item.kind {
            ItemKind::User { entry, ordinal } if item.matches(target, facts) => {
                Some((item.label("users", *ordinal), entry, *ordinal))
            }
            _ => None,
        })
}

/// Whether user `entry` writes its `authorized_keys`.
fn has_keys(entry: &UserEntry) -> bool {
    !entry.absent() && entry.authorized_keys.is_some()
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
    for (label, entry, _) in matched(expanded, target, facts) {
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
/// state or `authorized_keys`: `exclusive_groups` or `group` on the login
/// user, or removing a user without `force`. It prints the login user's
/// `id -Gn`, `id -G` and `id -gn` groups (`unknown` when `id` cannot tell)
/// and the gids and `/etc/group` members of `wheel`, `sudo` and `admin`
/// (`gid.GROUP=`, `members.GROUP=`); per user to remove
/// `busy.NAME=yes|no|unknown`: whether it owns a process, from `pgrep -u`,
/// or the effective uid of `/proc/*/status` without pgrep. It is `unknown`
/// when neither can tell, or when a non-root login cannot see other users'
/// processes (`/proc` mounted with `hidepid`, or a `/proc` whose mount
/// options cannot be read). A user that does not exist is `no`. Per user
/// with `authorized_keys` it prints `user.NAME=present|missing|unknown` and
/// the passwd home as `home.NAME=`; with `exclusive_keys` on the login user,
/// `login_keys=read|missing|unknown` for its `HOME/.ssh/authorized_keys`
/// and each line of it after `login_key=`.
pub fn query(expanded: &Expanded, target: &Target, facts: &Facts) -> Option<String> {
    query_with(expanded, target, facts, MOUNTS)
}

/// [`query`], reading `/proc`'s mount options from `mounts`.
fn query_with(expanded: &Expanded, target: &Target, facts: &Facts, mounts: &str) -> Option<String> {
    let login = facts.get("user");
    let mut body = String::new();
    let mut groups = false;
    let mut keys = false;
    for (_, entry, _) in matched(expanded, target, facts) {
        let name = entry.name.as_str();
        if has_keys(entry) {
            body += &format!(
                "anvil_l=$(anvil_pw {name}); case $? in \
                 0) echo user.{name}=present; echo \"home.{name}=$(printf '%s\\n' \"$anvil_l\" | cut -d: -f6)\" ;; \
                 2) echo user.{name}=missing ;; *) echo user.{name}=unknown ;; esac\n"
            );
        }
        if has_keys(entry) && entry.exclusive_keys && login == Some(name) && !keys {
            keys = true;
            body += &format!(
                "anvil_h=$(anvil_pw {name} | cut -d: -f6); anvil_f=\"$anvil_h/.ssh/authorized_keys\"\n\
                 if [ -z \"$anvil_h\" ]; then echo login_keys=unknown\n\
                 elif [ -e \"$anvil_f\" ]; then\n\
                 if anvil_k=$(cat -- \"$anvil_f\" 2>/dev/null); then echo login_keys=read\n\
                 printf '%s\\n' \"$anvil_k\" | sed 's/^/login_key=/'\n\
                 else echo login_keys=unknown; fi\n\
                 elif [ -x \"$anvil_h\" ] && {{ [ ! -e \"$anvil_h/.ssh\" ] || [ -x \"$anvil_h/.ssh\" ]; }}; then \
                 echo login_keys=missing\n\
                 else echo login_keys=unknown; fi\n"
            );
        }
        if entry.absent() && !entry.force {
            body += &format!("echo \"busy.{name}=$(anvil_busy {name})\"\n");
        } else if !entry.absent() && admin_guard(entry) && login == Some(name) && !groups {
            groups = true;
            body += &format!(
                "anvil_v=$(id -Gn {name} 2>/dev/null); echo \"login_groups=${{anvil_v:-unknown}}\"\n\
                 anvil_v=$(id -G {name} 2>/dev/null); echo \"login_gids=${{anvil_v:-unknown}}\"\n\
                 echo \"login_primary=$(id -gn {name} 2>/dev/null)\"\n"
            );
            for group in ADMIN_GROUPS {
                body += &format!(
                    "echo \"gid.{group}=$(anvil_group {group} 3)\"\n\
                     echo \"members.{group}=$(anvil_group {group} 4)\"\n"
                );
            }
        }
    }
    if body.is_empty() {
        return None;
    }
    Some(format!(
        "{{ LC_ALL=C; export LC_ALL; set -f\n\
         anvil_group() {{ {{ if command -v getent >/dev/null 2>&1; then getent group \"$1\"; \
         else awk -F: -v k=\"$1\" '$1 == k' /etc/group; fi; }} 2>/dev/null | cut -d: -f\"$2\"; }}\n\
         {ANVIL_PW}\
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
/// `admin` while it is a member, removing a user that owns a process
/// without `force`, and `exclusive_keys` on the login user listing none of
/// the keys its `authorized_keys` holds, or no key at all when it holds
/// none. The error is the target's failure; else the [`Homes`] of the users
/// with `authorized_keys`, the store's keys from `store`.
pub fn check_state(
    expanded: &Expanded,
    target: &Target,
    facts: &Facts,
    output: &[u8],
    store: &StoreKeys,
) -> Result<Homes, String> {
    let text = String::from_utf8_lossy(output);
    let lines = || text.lines().take_while(|line| *line != QUERY_END);
    let state: BTreeMap<&str, &str> = lines()
        .filter(|line| !line.starts_with("login_key="))
        .filter_map(|line| line.split_once('='))
        .collect();
    if !text.lines().any(|line| line == QUERY_END) {
        return Err("users: the account query output has no end marker".to_owned());
    }
    let login = facts.get("user");
    let mut homes = Homes::default();
    for (label, entry, i) in matched(expanded, target, facts) {
        let name = entry.name.as_str();
        if has_keys(entry) {
            let home = match (&entry.home, state.get(format!("user.{name}").as_str())) {
                (Some(home), _) => home.clone(),
                (None, Some(&"missing")) => entry.new_home(),
                (None, Some(&"present")) => match state.get(format!("home.{name}").as_str()) {
                    Some(home) if home.starts_with('/') => (*home).to_owned(),
                    _ => return Err(format!("{label} {name}: has no home for authorized_keys")),
                },
                (None, _) => return Err(format!("{label} {name}: cannot read the user")),
            };
            homes.0.insert(i, home);
        }
        if has_keys(entry) && entry.exclusive_keys && login == Some(name) {
            let wanted = listed(entry, i, store)?;
            let wanted: Vec<Key> = wanted
                .iter()
                .filter_map(|line| parse_key(line).ok())
                .collect();
            let read = || {
                let keys: Vec<&str> = (text.split('\n'))
                    .take_while(|line| *line != QUERY_END)
                    .filter_map(|line| line.strip_prefix("login_key="))
                    .collect();
                file_keys(&keys.join("\n"))
            };
            let have: Option<Vec<Key>> = match state.get("login_keys") {
                Some(&"read") => read(),
                Some(&"missing") => Some(Vec::new()),
                _ => None,
            };
            let Some(have) = have else {
                return Err(format!(
                    "{label} {name}: cannot tell which keys the login user signs in with"
                ));
            };
            let kept = if have.is_empty() {
                !wanted.is_empty()
            } else {
                have.iter().any(|key| wanted.contains(key))
            };
            if !kept {
                return Err(format!("{label} {name}: would drop the login key"));
            }
        }
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
        if !entry.absent() && admin_guard(entry) && login == Some(name) {
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
            let listed = |group: &str| entry.groups.iter().flatten().any(|kept| kept == group);
            let mut dropped = None;
            for group in ADMIN_GROUPS {
                if !member(group) || primary(group) || listed(group) {
                    continue;
                }
                if !entry.exclusive_groups {
                    let Some(members) = state.get(format!("members.{group}").as_str()) else {
                        return Err(format!(
                            "{label} {name}: cannot tell whether the login user is in wheel, sudo or admin"
                        ));
                    };
                    if members.split(',').any(|member| member == name) {
                        continue;
                    }
                }
                dropped = Some(group);
                break;
            }
            if let Some(group) = dropped {
                return Err(format!(
                    "{label} {name}: would drop the login user from {group}"
                ));
            }
        }
    }
    Ok(homes)
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
            "echo \"gid.wheel=$(anvil_group wheel 3)\"\n",
            "echo \"members.wheel=$(anvil_group wheel 4)\"\n",
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
    fn moving_the_login_users_primary_group_off_an_admin_group_is_refused() {
        let target = crate::test_util::target("web");
        let facts = login("deploy");
        let state = |text: &str, output: &str| {
            check_state(
                &users(&format!("[[users]]\nname = \"deploy\"\n{text}")),
                &target,
                &facts,
                output.as_bytes(),
                &StoreKeys::default(),
            )
            .map(|_| ())
        };
        let output = |members: &str| {
            format!(
                "login_groups=wheel docker\nlogin_gids=10 2001\nlogin_primary=wheel\n\
                 gid.wheel=10\n{members}gid.sudo=\nmembers.sudo=\ngid.admin=\nmembers.admin=\n\
                 anvil-users-end=1\n"
            )
        };
        let primary_only = output("members.wheel=\n");
        let dropped = Err("users[0] deploy: would drop the login user from wheel".to_owned());
        assert_eq!(state("group = \"app\"", &primary_only), dropped);
        assert_eq!(state("group = \"2000\"", &primary_only), dropped);
        assert_eq!(
            state("group = \"app\"", &output("")),
            Err(
                "users[0] deploy: cannot tell whether the login user is in wheel, sudo or admin"
                    .to_owned()
            )
        );
        for (text, output) in [
            ("group = \"app\"", output("members.wheel=web,deploy\n")),
            (
                "group = \"app\"\ngroups = [\"wheel\"]",
                primary_only.clone(),
            ),
            ("group = \"wheel\"", primary_only.clone()),
            ("group = \"10\"", primary_only.clone()),
            ("groups = [\"docker\"]", primary_only.clone()),
        ] {
            assert_eq!(state(text, &output), Ok(()), "{text}");
        }
        let script = query(
            &users("[[users]]\nname = \"deploy\"\ngroup = \"app\"\n"),
            &target,
            &facts,
        );
        assert!(
            script.is_some_and(
                |script| script.contains("echo \"members.wheel=$(anvil_group wheel 4)\"\n")
            ),
            "a primary group change queries the admin groups"
        );
    }

    #[test]
    fn state_guards_refuse_dropping_the_login_user_from_an_admin_group_or_removing_a_busy_user() {
        let target = crate::test_util::target("web");
        let facts = login("deploy");
        let state = |text: &str, output: &str| {
            check_state(
                &users(text),
                &target,
                &facts,
                output.as_bytes(),
                &StoreKeys::default(),
            )
            .map(|_| ())
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
    fn a_user_with_uid_0_is_never_removed() {
        for root in ["0", "00"] {
            let passwd = format!("root:x:0:0::/root:/bin/sh\ndeploy:x:{root}:0::/root:/bin/sh\n");
            for accounts in [Accounts::Shadow, Accounts::Busybox] {
                for keys in ["", "remove_home = true", "remove_home = true\nforce = true"] {
                    let absent = entry(&format!("state = \"absent\"\n{keys}"));
                    let (code, out, err, log) = Fake::new(&passwd).run(&absent, accounts);
                    assert_eq!(
                        (code, out.as_str(), err.as_str(), log.as_str()),
                        (Some(1), "", "users[0] deploy: has uid 0; refused\n", ""),
                        "{root} {keys}"
                    );
                }
            }
        }
    }

    #[test]
    fn remove_home_removes_only_a_home_the_user_owns_and_never_root() {
        let home = temp_dir("users-home");
        let me = my_uid();
        let passwd = |uid: &str, home: &str| {
            format!("root:x:0:0::/root:/bin/sh\ndeploy:x:{uid}:100:Deploy:{home}:/bin/sh\n")
        };
        let at = home.display().to_string();
        for (accounts, force, wanted) in [
            (Accounts::Shadow, "force = true", "userdel -f -r deploy\n"),
            (Accounts::Shadow, "", "userdel -r deploy\n"),
            (Accounts::Busybox, "", "deluser --remove-home deploy\n"),
            (
                Accounts::Busybox,
                "force = true",
                "deluser --remove-home deploy\n",
            ),
        ] {
            let keys = entry(&format!("state = \"absent\"\nremove_home = true\n{force}"));
            let (code, out, err, log) = Fake::new(&passwd("1500", &at)).run(&keys, accounts);
            assert_eq!((code, out.as_str(), log.as_str()), (Some(1), "", ""));
            assert_eq!(
                err,
                format!("users[0] deploy: home {at} is not owned by deploy; not removed\n")
            );
            for root in ["/", "//"] {
                let (code, out, err, log) = Fake::new(&passwd(&me, root)).run(&keys, accounts);
                assert_eq!(
                    (code, out.as_str(), err.as_str(), log.as_str()),
                    (Some(1), "", "users[0] deploy: home / is not removed\n", ""),
                    "{root}"
                );
            }
            let (code, out, err, log) = Fake::new(&passwd(&me, &at)).run(&keys, accounts);
            assert_eq!(
                (code, out.as_str(), log.as_str()),
                (Some(0), "removed user deploy\n", wanted),
                "{err}"
            );
        }
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

    /// An `ssh-ed25519` key line whose 32 key bytes are all `seed`.
    fn key(seed: u8, comment: &str) -> String {
        let kind = "ssh-ed25519";
        let mut blob = (kind.len() as u32).to_be_bytes().to_vec();
        blob.extend_from_slice(kind.as_bytes());
        blob.extend_from_slice(&32u32.to_be_bytes());
        blob.extend_from_slice(&[seed; 32]);
        format!("{kind} {} {comment}", crate::bytes::base64(&blob))
    }

    #[test]
    fn a_key_parses_as_one_openssh_line_with_or_without_options() {
        let a = key(1, "me@host");
        let parsed = parse_key(&a).unwrap();
        assert_eq!(parse_key(&key(1, "other")), Ok(parsed.clone()));
        let with_options = format!("from=\"10.0.0.1,a b\",no-pty {a}");
        assert_eq!(parse_key(&with_options), Ok(parsed.clone()));
        assert_eq!(parse_key(a.trim_end_matches(" me@host")), Ok(parsed));
        assert_ne!(parse_key(&key(2, "")), parse_key(&key(1, "")));
        let blob = a.split(' ').nth(1).unwrap();
        for (line, wanted) in [
            (format!("{a}\n{a}"), "must be one line"),
            ("not a key".to_owned(), "is not an OpenSSH public key line"),
            (
                "ssh-ed25519-cert-v01@openssh.com AAAA".to_owned(),
                "is not an OpenSSH public key line",
            ),
            (
                "ssh-ed25519 !!!!".to_owned(),
                "has no base64 key after its type",
            ),
            ("ssh-ed25519".to_owned(), "has no base64 key after its type"),
            (format!("ssh-rsa {blob}"), "has a key that is not ssh-rsa"),
        ] {
            assert_eq!(parse_key(&line), Err(wanted.to_owned()), "{line}");
        }
    }

    #[test]
    fn a_bad_inline_key_or_exclusive_keys_alone_names_the_entry() {
        let user = |keys: &str| crate::config::check_user("users[0]", &entry(keys));
        let a = key(1, "a");
        assert_eq!(
            user(&format!(
                "authorized_keys = [\"{a}\", \"$/keys/b.pub\"]\nexclusive_keys = true"
            )),
            Ok(())
        );
        for (keys, wanted) in [
            (
                format!("authorized_keys = [\"{a}\", \"ssh-rsa x\"]"),
                "users[0].authorized_keys[1] `ssh-rsa x`: has no base64 key after its type",
            ),
            (
                "exclusive_keys = true".to_owned(),
                "users[0].exclusive_keys: needs authorized_keys",
            ),
            (
                format!("state = \"absent\"\nauthorized_keys = [\"{a}\"]"),
                "users[0].authorized_keys: not with state = \"absent\"",
            ),
        ] {
            assert_eq!(user(&keys), Err(wanted.to_owned()), "{keys}");
        }
        assert_eq!(
            shown(&entry(&format!(
                "authorized_keys = [\"{a}\", \"{a}\"]\nexclusive_keys = true"
            ))),
            "user deploy (authorized_keys 2, exclusive)"
        );
        assert_eq!(
            shown(&entry(&format!(
                "shell = \"/bin/sh\"\nauthorized_keys = [\"{a}\"]"
            ))),
            "user deploy (shell /bin/sh, authorized_keys 1)"
        );
    }

    fn write(keys: &[String], exclusive: bool) -> KeysWrite {
        KeysWrite {
            label: "users[0] deploy".to_owned(),
            user: "deploy".to_owned(),
            keys: keys.to_vec(),
            exclusive,
            wrap: String::new(),
        }
    }

    fn file(text: &str) -> Captured {
        Captured::File {
            mode: "0600".to_owned(),
            uid: 1500,
            gid: 100,
            size: text.len() as u64,
            sha256: String::new(),
            bytes: Some(Zeroizing::new(text.as_bytes().to_vec())),
            reason: None,
        }
    }

    #[test]
    fn listed_keys_are_added_once_and_exclusive_keys_replace_the_file() {
        let (a, b) = (key(1, "a"), key(2, "b"));
        let dest = "/home/deploy/.ssh/authorized_keys";
        let current = format!("# mine\nno-pty {a}");
        let add = write(&[key(1, "renamed"), b.clone(), b.clone()], false);
        assert_eq!(
            add.merged(dest, &file(&current)),
            Ok(format!("{current}\n{b}\n")),
            "a key already there under other options or comment is kept as it is"
        );
        let once = add.merged(dest, &file(&current)).unwrap();
        assert_eq!(add.merged(dest, &file(&once)), Ok(once.clone()));
        assert_eq!(
            add.merged(dest, &Captured::Absent),
            Ok(format!("{}\n{b}\n", key(1, "renamed")))
        );
        let exclusive = write(&[b.clone(), b.clone()], true);
        assert_eq!(
            exclusive.merged(dest, &file(&current)),
            Ok(format!("{b}\n"))
        );
        assert_eq!(
            write(&[], true).merged(dest, &file(&current)),
            Ok(String::new())
        );
        let unread = Captured::File {
            mode: "0600".to_owned(),
            uid: 1500,
            gid: 100,
            size: 300 * 1024,
            sha256: String::new(),
            bytes: None,
            reason: Some("over 256 KiB"),
        };
        assert_eq!(
            add.merged(dest, &unread),
            Err(format!(
                "users[0] deploy: cannot read {dest} (over 256 KiB)"
            ))
        );
    }

    #[test]
    fn the_write_runs_as_the_user_unless_it_is_the_login() {
        let facts = |user: &str, sudo: &str| {
            Facts(BTreeMap::from([
                ("user".to_owned(), user.to_owned()),
                ("sudo".to_owned(), sudo.to_owned()),
            ]))
        };
        let as_deploy = |f: &Facts, sudo: bool| as_user("users[0] deploy", "deploy", f, sudo);
        assert_eq!(
            as_deploy(&facts("deploy", "none"), false),
            Ok(String::new())
        );
        assert_eq!(
            as_deploy(&facts("root", "none"), false),
            Ok("su -s /bin/sh deploy".to_owned())
        );
        assert_eq!(
            as_deploy(&facts("cmj", "nopasswd"), true),
            Ok("sudo -n -u deploy /bin/sh".to_owned())
        );
        assert_eq!(
            as_deploy(&facts("cmj", "nopasswd"), false),
            Err("users[0] deploy: authorized_keys of another user needs sudo".to_owned())
        );
        assert!(as_deploy(&facts("cmj", "binary"), true).is_err());
        let script = "echo hi\nexit 3\n";
        let wrapped = wrapped("sh", script);
        assert!(wrapped.starts_with("sh <<'ANVIL_AS_"), "{}", *wrapped);
        let out = Local.run(&wrapped, RUN_TIMEOUT).unwrap();
        assert_eq!((out.code, out.stdout), (Some(3), b"hi\n".to_vec()));
        assert_eq!(*crate::users::wrapped("", script), script);
    }

    #[test]
    fn exclusive_keys_on_the_login_user_must_keep_one_of_its_keys() {
        let target = crate::test_util::target("web");
        let facts = login("deploy");
        let (a, b) = (key(1, "a"), key(2, "b"));
        let config = |keys: &[&str], name: &str| {
            let list: Vec<String> = keys.iter().map(|k| format!("\"{k}\"")).collect();
            format!(
                "[[users]]\nname = \"{name}\"\nauthorized_keys = [{}]\nexclusive_keys = true\n",
                list.join(", ")
            )
        };
        let state = |text: &str, output: &str| {
            check_state(
                &users(text),
                &target,
                &facts,
                output.as_bytes(),
                &StoreKeys::default(),
            )
        };
        let state_ok = |text: &str, output: &str| state(text, output).map(|_| ());
        let read = format!(
            "user.deploy=present\nhome.deploy=/home/deploy\nlogin_keys=read\n\
             login_key=# a comment\nlogin_key=no-pty {a}\nanvil-users-end=1\n"
        );
        let refused = Err("users[0] deploy: would drop the login key".to_owned());
        assert_eq!(state_ok(&config(&[&b], "deploy"), &read), refused);
        assert_eq!(state_ok(&config(&[], "deploy"), &read), refused);
        let kept = state(&config(&[&key(1, "renamed"), &b], "deploy"), &read).unwrap();
        assert_eq!(kept.0, BTreeMap::from([(0, "/home/deploy".to_owned())]));
        for listed in [&[][..], &[a.as_str()][..]] {
            let output = |keys: &str| {
                format!("user.deploy=present\nhome.deploy=/home/deploy\n{keys}anvil-users-end=1\n")
            };
            let wanted = if listed.is_empty() {
                refused.clone()
            } else {
                Ok(())
            };
            for keys in ["login_keys=missing\n", "login_keys=read\nlogin_key=\n"] {
                assert_eq!(
                    state_ok(&config(listed, "deploy"), &output(keys)),
                    wanted,
                    "{keys}"
                );
            }
            assert_eq!(
                state_ok(&config(listed, "deploy"), &output("login_keys=unknown\n")),
                Err(
                    "users[0] deploy: cannot tell which keys the login user signs in with"
                        .to_owned()
                )
            );
        }
        assert_eq!(
            state(&config(&[], "web"), "user.web=missing\nanvil-users-end=1\n").map(|h| h.0),
            Ok(BTreeMap::from([(0, "/home/web".to_owned())])),
            "not the login user: a new user's default home"
        );
        assert_eq!(
            state_ok(&config(&[], "web"), "user.web=unknown\nanvil-users-end=1\n"),
            Err("users[0] web: cannot read the user".to_owned())
        );
    }

    #[test]
    fn the_query_reads_homes_and_the_login_users_keys() {
        let dir = temp_dir("users-keys");
        let me = whoami();
        let a = key(1, "a");
        fs::create_dir_all(dir.join("home/.ssh")).unwrap();
        fs::write(dir.join("home/.ssh/authorized_keys"), format!("{a}\n")).unwrap();
        let passwd = format!("{me}:x:1500:100::{}/home:/bin/sh\n", dir.display());
        fs::write(dir.join("passwd"), passwd).unwrap();
        let getent = format!(
            "#!/bin/sh\nawk -F: -v k=\"$2\" '$1 == k {{ print; f = 1 }} END {{ exit f ? 0 : 2 }}' '{}'/passwd\n",
            dir.display()
        );
        fs::write(dir.join("getent"), getent).unwrap();
        chmod(&dir.join("getent"), 0o755);
        let text = format!(
            "[[users]]\nname = \"{me}\"\nauthorized_keys = [\"{a}\"]\nexclusive_keys = true\n\
             [[users]]\nname = \"anvil_nobody_x\"\nauthorized_keys = []\n"
        );
        let expanded = users(&text);
        let (target, facts) = (crate::test_util::target("web"), login(&me));
        let script = query(&expanded, &target, &facts).unwrap();
        let path = format!("{}:{}", dir.display(), std::env::var("PATH").unwrap());
        let out = Local
            .run(&format!("PATH='{path}'\n{script}"), RUN_TIMEOUT)
            .unwrap();
        let output = String::from_utf8(out.stdout).unwrap().replace(&me, "ME");
        assert_eq!(
            output.replace(&dir.display().to_string(), "DIR"),
            format!(
                "user.ME=present\nhome.ME=DIR/home\nlogin_keys=read\nlogin_key={a}\n\
                 user.anvil_nobody_x=missing\nanvil-users-end=1\n"
            )
        );
        let output = output.replace("ME", &me);
        let homes = check_state(
            &expanded,
            &target,
            &facts,
            output.as_bytes(),
            &StoreKeys::default(),
        );
        assert_eq!(
            homes.map(|h| h.0),
            Ok(BTreeMap::from([
                (0, format!("{}/home", dir.display())),
                (1, "/home/anvil_nobody_x".to_owned())
            ]))
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_keys_step_follows_as_a_file_step_and_rolls_back_as_the_user() {
        let target = crate::test_util::target("web");
        let a = key(1, "a");
        let text = format!("[[users]]\nname = \"deploy\"\nauthorized_keys = [\"{a}\"]\n");
        let expanded = users(&text);
        let item = &expanded.items[0];
        let ItemKind::User { entry, .. } = &item.kind else {
            panic!("a user item")
        };
        let mut facts = login("root");
        facts.0.insert("accounts".to_owned(), "shadow".to_owned());
        facts
            .0
            .insert("accounts_bin".to_owned(), "/usr/sbin".to_owned());
        let homes = Homes(BTreeMap::from([(0, "/srv/deploy/".to_owned())]));
        let run = |homes: Option<&Homes>| {
            keys_run(
                item,
                (entry, 0),
                &target,
                Some(&facts),
                (homes, &Sources::default()),
            )
        };
        assert_eq!(run(None), Ok(None), "plan has no keys step");
        let planned = run(Some(&homes)).unwrap().unwrap();
        let dest = "/srv/deploy/.ssh/authorized_keys";
        assert_eq!(planned.shown(), format!("file {dest}"));
        let write = planned.file().unwrap().write.as_ref().unwrap();
        assert_eq!((write.dest.as_str(), write.mode.as_str()), (dest, "600"));
        assert!(!write.as_root && write.sudo.is_empty() && write.walk.private);
        let keys = write.keys.as_ref().unwrap();
        assert_eq!(keys.wrap, "su -s /bin/sh deploy");
        assert_eq!(keys.keys, vec![a.clone()]);
        let capture = write.capture();
        assert!(
            capture.starts_with("su -s /bin/sh deploy <<'ANVIL_AS_"),
            "{}",
            *capture
        );
        assert!(
            capture.contains("users[0] deploy: home /srv/deploy/ does not exist"),
            "{}",
            *capture
        );
        assert!(!capture.contains("-??????r*"), "any mode is read");
        let merged = write.merged(&Captured::Absent).unwrap().unwrap();
        let (script, upload) = crate::files::write_script(
            &merged,
            &Sources::default(),
            &crate::files::Expected::Absent,
        )
        .unwrap();
        assert!(upload.is_none());
        assert!(
            script.starts_with("su -s /bin/sh deploy <<'ANVIL_AS_"),
            "{}",
            *script
        );
        assert!(
            script.contains("mkdir -m 0700 '/srv/deploy/.ssh'"),
            "{}",
            *script
        );
        assert!(script.contains(&a), "{}", *script);

        let record = crate::record::FileRecord {
            user: Some("deploy".to_owned()),
            ..merged.record(
                &expanded,
                &Sources::default(),
                Captured::Absent,
                crate::record::Write::Written,
                false,
            )
        };
        assert_eq!((record.sudo, record.mode.as_str()), (false, "0600"));
        let config = toml::from_str::<Config>(&text).unwrap();
        let restore = |facts: &Facts, sudo: Option<bool>| {
            let sudo = sudo.map(|sudo| ("deploy".to_owned(), sudo));
            crate::files::restore_runs(
                &config,
                &target,
                facts,
                (std::slice::from_ref(&record), &sudo.into_iter().collect()),
                &crate::files::Force::None,
                crate::files::Wording::Cli,
            )
            .map(|runs| runs[0].run.to_string())
        };
        let run = restore(&facts, Some(false)).unwrap();
        assert!(
            run.starts_with("su -s /bin/sh deploy <<'ANVIL_AS_"),
            "{run}"
        );
        assert!(!run.contains("sudo -n"), "{run}");
        let mut other = facts.clone();
        other.0.insert("user".to_owned(), "cmj".to_owned());
        other.0.insert("sudo".to_owned(), "nopasswd".to_owned());
        let run = restore(&other, Some(true)).unwrap();
        assert!(
            run.starts_with("sudo -n -u deploy /bin/sh <<'ANVIL_AS_"),
            "{run}"
        );
        assert_eq!(
            restore(&other, Some(false)),
            Err(format!(
                "files: {dest}: authorized_keys of another user needs sudo"
            )),
            "the entry's sudo, not always true"
        );
        assert!(restore(&facts, None).is_err());
        let forged = crate::record::FileRecord {
            sudo: true,
            ..record.clone()
        };
        let mut password = other.clone();
        password.0.insert("sudo".to_owned(), "password".to_owned());
        for facts in [&facts, &other, &password] {
            let runs = crate::files::restore_runs(
                &config,
                &target,
                facts,
                (
                    std::slice::from_ref(&forged),
                    &[("deploy".to_owned(), true)].into(),
                ),
                &crate::files::Force::None,
                crate::files::Wording::Cli,
            )
            .unwrap();
            let run = runs[0].run.to_string();
            let inner = run.split_once('\n').map_or("", |(_, inner)| inner);
            assert!(
                !inner.contains("sudo -n") && !inner.contains("anvil_sudo='sudo"),
                "a record's sudo is not used: {run}"
            );
            assert!(runs[0].sudo_secret.is_none(), "{run}");
        }
    }

    #[test]
    fn a_commented_key_is_no_key_anywhere() {
        let (a, b) = (key(1, "a"), key(2, "b"));
        for line in [format!("# {a}"), format!("  #{a}")] {
            assert_eq!(parse_key(&line), Err("is a comment, not a key".to_owned()));
        }
        assert_eq!(
            crate::config::check_user(
                "users[0]",
                &entry(&format!("authorized_keys = [\"# {a}\"]"))
            ),
            Err(format!(
                "users[0].authorized_keys[0] `{}`: is a comment, not a key",
                shown_key(&format!("# {a}"))
            ))
        );
        let dest = "/home/deploy/.ssh/authorized_keys";
        let current = format!("# {b}\n{a}\n");
        assert_eq!(
            write(std::slice::from_ref(&b), false).merged(dest, &file(&current)),
            Ok(format!("{current}{b}\n")),
            "a commented copy is not present"
        );
        assert_eq!(file_keys(&current), Some(vec![parse_key(&a).unwrap()]));
        assert_eq!(file_keys(&format!("{a}\r\n")), None);
        assert_eq!(file_keys("ssh-foo AAAA\n"), None);
    }

    #[test]
    fn the_login_key_guard_ignores_comments_and_cannot_tell_past_a_bad_line() {
        let target = crate::test_util::target("web");
        let facts = login("deploy");
        let (a, b) = (key(1, "a"), key(2, "b"));
        let text = format!(
            "[[users]]\nname = \"deploy\"\nauthorized_keys = [\"{b}\"]\nexclusive_keys = true\n"
        );
        let state = |lines: &str| {
            check_state(
                &users(&text),
                &target,
                &facts,
                format!(
                    "user.deploy=present\nhome.deploy=/home/deploy\nlogin_keys=read\n{lines}anvil-users-end=1\n"
                )
                .as_bytes(),
                &StoreKeys::default(),
            )
            .map(|_| ())
        };
        assert_eq!(
            state(&format!("login_key=# {b}\nlogin_key=\nlogin_key={a}\n")),
            Err("users[0] deploy: would drop the login key".to_owned()),
            "a commented match does not count"
        );
        assert_eq!(state(&format!("login_key=# {a}\nlogin_key={b}\n")), Ok(()));
        assert_eq!(
            state(&format!(
                "login_key={a}\nlogin_key=# anvil-users-end=1\nlogin_key={b}\n"
            )),
            Ok(()),
            "only a line that is exactly the end marker ends the keys"
        );
        let unknown =
            Err("users[0] deploy: cannot tell which keys the login user signs in with".to_owned());
        for bad in [
            format!("login_key={b}\r\n"),
            "login_key=ssh-foo AAAA\n".to_owned(),
        ] {
            assert_eq!(state(&format!("login_key={b}\n{bad}")), unknown, "{bad:?}");
        }
    }
}
