//! What `[[groups]]` and `[[users]]` share: the account tools a target has,
//! the step each entry becomes ([`run`]), and the opening of its script
//! ([`prelude`]).

use crate::config::Config;
use crate::facts::Facts;
use crate::files::sudo_for;
use crate::inventory::Target;
use crate::remote::fail;
use crate::step::{PlannedRun, RunKind};
use crate::tasks::Item;
use crate::transport::RUN_TIMEOUT;

/// The account tools a target has, from its `accounts` fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accounts {
    /// shadow-utils: `groupadd`, `useradd` and the rest.
    Shadow,
    /// busybox: `addgroup`, `adduser` and the rest; fewer fields change.
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

/// A `[[groups]]` or `[[users]]` entry, as [`run`] turns it into a step.
pub(crate) trait Account {
    /// The section, `groups` or `users`, which starts the step's failures.
    const SECTION: &'static str;
    /// What one entry is, `group` or `user`.
    const NOUN: &'static str;

    fn name(&self) -> &str;
    fn sudo(&self) -> bool;
    /// What the step shows, from the config alone.
    fn shown(&self) -> String;
    /// The script that brings the entry to its state; `label` names the
    /// entry in its failures and `sudo` is "" (root), "sudo -n " or "sudo ".
    fn script(&self, label: &str, tools: (Accounts, &str), sudo: &str) -> String;
    /// The step's kind, a skip when `skip`.
    fn kind(name: String, skip: bool) -> RunKind;
}

/// The failure of a step of `section` on a target with no account tools.
pub(crate) fn no_tools(section: &str) -> String {
    format!("{section}: no account tools on this target (accounts = none)")
}

/// The step of account `item`, holding `entry`, the `i`th item of its
/// section: a skip when the entry does not match, a failure without account
/// tools, else its script, run as root or through sudo, with the password
/// typed when sudo asks for one.
pub(crate) fn run<A: Account>(
    config: &Config,
    item: &Item,
    (entry, i): (&A, usize),
    target: &Target,
    facts: Option<&Facts>,
) -> Result<PlannedRun, String> {
    let (section, noun) = (A::SECTION, A::NOUN);
    let facts = facts.ok_or_else(|| format!("{section}: the probe has not run"))?;
    let label = item.label(section, i);
    let name = entry.name();
    let step = |run: String, label: String, sudo_secret: Option<&str>, skip: bool| PlannedRun {
        run: run.into(),
        label: Some(label),
        sudo_secret: sudo_secret.map(str::to_owned),
        timeout: RUN_TIMEOUT,
        kind: A::kind(name.to_owned(), skip),
        task: None,
        name: None,
        notify: Vec::new(),
    };
    if !item.matches(target, facts) {
        log::info!("{}: {label} {name}: no match", target.name);
        let shown = format!("skipped {noun} {name}: no match");
        return Ok(step(String::new(), shown, None, true));
    }
    let Some((accounts, bin)) = Accounts::of(facts) else {
        log::info!("{}: {label} {name}: no account tools", target.name);
        let run = format!("{{ {}\n}}\n", fail(&no_tools(section)));
        return Ok(step(run, entry.shown(), None, false));
    };
    let (sudo, secret) = sudo_for(config, target, facts, entry.sudo());
    log::info!("{}: {label} {name} with {accounts:?} in {bin}", target.name);
    let run = entry.script(&label, (accounts, bin), sudo);
    Ok(step(run, entry.shown(), secret, false))
}

/// Defines `anvil_get DB KEY FIELD`, which prints the line of `/etc/DB`
/// whose FIELD is KEY, with getent when the target has it, and returns 2
/// when there is none.
pub(crate) const ANVIL_GET: &str = "anvil_g=; command -v getent >/dev/null 2>&1 && anvil_g=1\n\
     anvil_get() { if [ -n \"$anvil_g\" ]; then getent \"$1\" \"$2\"; else \
     awk -F: -v k=\"$2\" -v f=\"$3\" '$f == k { print; n = 1; exit } END { exit n ? 0 : 2 }' \
     \"/etc/$1\"; fi; }\n";

/// What opens every account script: the brace group, C collation, no
/// globbing.
pub(crate) const OPEN: &str = "{ LC_ALL=C; export LC_ALL; set -f\n";

/// The opening of the script of account `A` named `name`: [`OPEN`], `sudo
/// true` when `sudo` is set, so that a sudo failure is reported once, as
/// sudo's message, [`ANVIL_GET`], and `name`'s line of `/etc/DB` in
/// `anvil_line`, empty when there is none.
pub(crate) fn prelude<A: Account>(db: &str, name: &str, sudo: &str) -> String {
    let check = if sudo.is_empty() {
        String::new()
    } else {
        format!("{sudo}true || exit\n")
    };
    format!(
        "{OPEN}{check}{ANVIL_GET}\
         anvil_line=$(anvil_get {db} {name} 1); anvil_rc=$?\n\
         case $anvil_rc in 0) ;; 2) anvil_line= ;; *) {unread} ;; esac\n",
        unread = fail(&format!("{}: cannot read {} {name}", A::SECTION, A::NOUN)),
    )
}

/// The lines of `/etc/passwd` without getent, for a scan of every user.
pub(crate) const ALL_PASSWD: &str =
    "{ if [ -n \"$anvil_g\" ]; then getent passwd; else cat /etc/passwd; fi; }";
