//! `[[services]]`: a service brought to a state, and to an enablement when
//! one is set, by the init each target runs.
//!
//! [`Builder::run`] turns each entry into one step per target: a skip when the
//! entry does not match, else the script, run as root or through
//! `sudo -n`, with the password typed when sudo asks for one.
//!
//! [`init_for`] names the init from the `distro` fact. [`script`] builds one
//! sh brace group that asks the init first and changes only what differs: it
//! prints one line per change (`enabled`, `disabled`, `started`, `stopped`,
//! then `service NAME`) or `unchanged service NAME`, and fails the step when
//! the init does not know the service. The actions `restarted` and
//! `reloaded` run every time and print `restarted service NAME` or
//! `reloaded service NAME`, or `service NAME is not running; not reloaded`
//! for a reload of a service that is not running.

use std::fmt;
use std::time::Duration;

use crate::config::{Config, ServiceEntry, ServiceState, check_service_name};
use crate::facts::Facts;
use crate::files::{self, quote};
use crate::inventory::Target;
use crate::packages::Manager;
use crate::secret;
use crate::step::{PlannedRun, RunKind, ServiceRun};
use crate::tasks::Item;
use crate::transport::RUN_TIMEOUT;

/// The time limit of a service step: systemd's default 90 s start timeout
/// plus the password exchange.
pub const SERVICE_TIMEOUT: Duration = Duration::from_secs(120);

/// Where launchd's system daemons keep their plists.
pub const LAUNCH_DAEMONS: &str = "/Library/LaunchDaemons";

/// The init a target runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Init {
    Systemd,
    OpenRc,
    Launchd,
}

impl fmt::Display for Init {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Init::Systemd => "systemd",
            Init::OpenRc => "OpenRC",
            Init::Launchd => "launchd",
        })
    }
}

/// The init `distro` (the probe's os-release ID, or `macos`) runs: launchd
/// on macOS, OpenRC on Alpine, and systemd on every distro that `[packages]`
/// installs with pacman, apt or dnf.
pub fn init_for(distro: &str) -> Option<Init> {
    match Manager::for_distro(distro)? {
        Manager::Brew => Some(Init::Launchd),
        Manager::Apk => Some(Init::OpenRc),
        Manager::Pacman | Manager::Apt | Manager::Dnf => Some(Init::Systemd),
    }
}

/// Builds the steps of one target's service items, in list order: a skip when
/// the item does not match, else its script under the target's init. The init
/// and the way to root are looked up once, and only when an item matches;
/// either failing fails the whole target.
pub struct Builder<'a> {
    config: &'a Config,
    target: &'a Target,
    facts: Option<&'a Facts>,
    found: Option<(Init, &'static str, Option<&'a str>)>,
    seen: usize,
}

impl<'a> Builder<'a> {
    pub fn new(config: &'a Config, target: &'a Target, facts: Option<&'a Facts>) -> Builder<'a> {
        Builder {
            config,
            target,
            facts,
            found: None,
            seen: 0,
        }
    }

    /// The step of service `item`, holding `entry`.
    pub fn run(&mut self, item: &Item, entry: &ServiceEntry) -> Result<PlannedRun, String> {
        let target = self.target;
        let facts = self.facts.ok_or("services: the probe has not run")?;
        let i = self.seen;
        self.seen += 1;
        let label = item.label("services", i);
        let name = &entry.name;
        if !item.matches(target, facts) {
            log::info!("{}: {label} {name}: no match", target.name);
            return Ok(service_run(
                name,
                String::new(),
                format!("skipped service {name}: no match"),
                None,
                true,
            ));
        }
        let (init, sudo, secret) = match self.found {
            Some(found) => found,
            None => {
                let init = init_of(facts)?;
                let (sudo, secret) = root_access(self.config, target, facts)?;
                *self.found.insert((init, sudo, secret))
            }
        };
        let state = entry
            .state()
            .ok_or_else(|| format!("services: internal error: {label} has no valid state"))?;
        match secret {
            Some(secret) => log::info!(
                "{}: {label} {name} with {init}, sudo password from secret {secret}",
                target.name
            ),
            None => log::info!("{}: {label} {name} with {init}", target.name),
        }
        Ok(service_run(
            name,
            script(init, name, state, entry.enabled, sudo, LAUNCH_DAEMONS),
            format!("service {name}"),
            secret.map(str::to_owned),
            false,
        ))
    }
}

/// The init the target's `distro` fact names.
fn init_of(facts: &Facts) -> Result<Init, String> {
    let distro = facts.get("distro").unwrap_or("");
    init_for(distro).ok_or_else(|| {
        format!(
            "services: no init known for distro `{}`",
            secret::shown_name(distro)
        )
    })
}

/// How a service step reaches root on `target`: the script's sudo prefix,
/// and the secret whose password is typed when sudo asks for one.
fn root_access<'a>(
    config: &'a Config,
    target: &'a Target,
    facts: &Facts,
) -> Result<(&'static str, Option<&'a str>), String> {
    if facts.is_root() {
        return Ok(("", None));
    }
    match facts.get("sudo") {
        Some("nopasswd") => Ok(("sudo -n ", None)),
        Some("binary") => match files::sudo_secret_for(config, target) {
            Some(secret) => Ok(("sudo -n ", Some(secret))),
            None => Err("services: sudo asks for a password; set sudo_secret \
                 (local_sudo_secret for local), set NOPASSWD, or connect as root"
                .to_string()),
        },
        _ => Err("services: needs root and sudo is not installed".to_string()),
    }
}

fn service_run(
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
        timeout: if skip { RUN_TIMEOUT } else { SERVICE_TIMEOUT },
        kind: RunKind::Service {
            name: name.to_owned(),
            run: ServiceRun { skip },
        },
        task: None,
        name: None,
        notify: Vec::new(),
    }
}

/// One sh brace group that brings `name` to `state` and, when `enabled` is
/// set, that enablement, under `init`. `sudo` is "" (root) or "sudo -n ".
/// `plists` is the launchd plist directory, [`LAUNCH_DAEMONS`] outside tests.
///
/// `name` is inserted bare: the config's charset needs no quoting, and it
/// never starts with `-`. With `sudo` set, `sudo -n true` runs first, so a
/// sudo failure is reported once, as sudo's message. A query that fails
/// fails the step; the tools' own output of a change goes to stderr, so
/// stdout holds only the script's lines.
///
/// The actions `restarted` and `reloaded` take no `enabled` and do not
/// compare: they run every time, printing `restarted service NAME` or
/// `reloaded service NAME`, or, for a reload of a service that is not
/// running, the line that it was not reloaded, never `unchanged`.
pub fn script(
    init: Init,
    name: &str,
    state: ServiceState,
    enabled: Option<bool>,
    sudo: &str,
    plists: &str,
) -> String {
    debug_assert!(check_service_name(name).is_ok(), "{name}");
    let check = if sudo.is_empty() {
        ""
    } else {
        "sudo -n true || exit\n"
    };
    let up = match state {
        ServiceState::Started => true,
        ServiceState::Stopped => false,
        ServiceState::Restarted | ServiceState::Reloaded => {
            debug_assert!(enabled.is_none(), "{name}: enabled with an action");
            let reload = state == ServiceState::Reloaded;
            let body = match init {
                Init::Systemd => systemd_action(name, reload, sudo),
                Init::OpenRc => openrc_action(name, reload, sudo),
                Init::Launchd => launchd_action(name, reload, sudo, plists),
            };
            return format!("{{ up= off=\n{check}{body}}}\n");
        }
    };
    if init == Init::Launchd && up && enabled == Some(false) {
        let refusal = fail(&format!(
            "services: {name}: started with enabled = false cannot last on launchd"
        ));
        return format!("{{ {refusal}\n}}\n");
    }
    let body = match init {
        Init::Systemd => systemd(name, up, enabled, sudo),
        Init::OpenRc => openrc(name, up, enabled, sudo),
        Init::Launchd => launchd(name, up, enabled, sudo, plists),
    };
    let unchanged = quote(&format!("unchanged service {name}"));
    format!("{{ c= up= on= off= e= s=\n{check}{body}[ -n \"$c\" ] || echo {unchanged}\n}}\n")
}

/// `command` with its stdout on stderr, then `reloaded service NAME` (with
/// `reload`) or `restarted service NAME`; a failure exits with its status.
fn act(name: &str, reload: bool, command: &str) -> String {
    let word = if reload { "reloaded" } else { "restarted" };
    format!(
        "{command} >&2 && echo {} || exit\n",
        quote(&format!("{word} service {name}"))
    )
}

/// The reload of a service when `up` is set; else the line that it is not
/// running, with no call.
fn reload_if_up(name: &str, command: &str) -> String {
    format!(
        "if [ -n \"$up\" ]; then {}else echo {}\nfi\n",
        act(name, true, command),
        quote(&format!("service {name} is not running; not reloaded"))
    )
}

/// `message` on stderr, then exit 1.
fn fail(message: &str) -> String {
    format!("{{ echo {} >&2; exit 1; }}", quote(message))
}

/// `command` with its stdout on stderr, then each of `lines` on stdout,
/// marking that something changed; a failure exits with `command`'s status.
fn change(command: &str, lines: &[String]) -> String {
    let echoes: String = lines
        .iter()
        .map(|line| format!(" && echo {}", quote(line)))
        .collect();
    format!("{command} >&2{echoes} || exit; c=1")
}

/// The stdout lines of an enablement and a state change.
fn lines(name: &str, up: bool, enabled: Option<bool>) -> (String, String) {
    let enablement = match enabled {
        Some(true) => "enabled",
        _ => "disabled",
    };
    let state = if up { "started" } else { "stopped" };
    (
        format!("{enablement} service {name}"),
        format!("{state} service {name}"),
    )
}

/// Sets `s` when the service's running state, `$up`, is not `up`.
fn state_differs(up: bool) -> &'static str {
    if up {
        "[ -n \"$up\" ] || s=1\n"
    } else {
        "[ -z \"$up\" ] || s=1\n"
    }
}

/// The enablement change when `e` is set, then the state change when `s`
/// is; with `both`, a single command when `e` is 1 and `s` is set.
fn changes(
    enablement: Option<&str>,
    state: &str,
    both: Option<&str>,
    (enable_line, state_line): (String, String),
) -> String {
    let separate = enablement
        .map(|command| {
            format!(
                "if [ -n \"$e\" ]; then {}; fi\n",
                change(command, std::slice::from_ref(&enable_line))
            )
        })
        .unwrap_or_default()
        + &format!(
            "if [ -n \"$s\" ]; then {}; fi\n",
            change(state, std::slice::from_ref(&state_line))
        );
    match both {
        Some(command) => format!(
            "if [ \"$e\" = 1 ] && [ -n \"$s\" ]; then {}\nelse\n{separate}fi\n",
            change(command, &[enable_line, state_line])
        ),
        None => separate,
    }
}

/// systemd: `LoadState` first (`show` without `--value`, which systemd
/// before 230 lacks), then `is-active` and, only when `enabled` is set,
/// `is-enabled`, whose non-zero exit is a failure only outside its
/// documented states. `enable --now` or `disable --now` when both differ the
/// same way; else enablement, then state. A runtime-only enablement is
/// disabled with `--runtime`, since a plain `disable` leaves its /run link.
fn systemd(name: &str, up: bool, enabled: Option<bool>, sudo: &str) -> String {
    let sc = |args: &str| systemctl(sudo, args, name);
    let mut s = systemd_found(name, up || enabled == Some(true), sudo);
    s += &format!("{} && up=1\n", sc("is-active --quiet"));
    if let Some(on) = enabled {
        s += &format!(
            "on=$({}) || {{ r=$?; case $on in \
             disabled|linked|linked-runtime|masked|masked-runtime|bad) ;; *) exit $r ;; esac; }}\n",
            sc("is-enabled")
        );
        s += if on {
            "case $on in enabled|static|indirect|generated|alias) ;; *) e=1 ;; esac\n"
        } else {
            "case $on in enabled) e=1 ;; enabled-runtime) e=runtime ;; esac\n"
        };
    }
    s += state_differs(up);
    let verb = if up { "start" } else { "stop" };
    let verb_of = |on: bool| if on { "enable" } else { "disable" };
    let enablement = enabled.map(|on| {
        if on {
            sc("enable")
        } else {
            format!(
                "if [ \"$e\" = runtime ]; then {}; else {}; fi",
                sc("disable --runtime"),
                sc("disable")
            )
        }
    });
    let both = enabled
        .filter(|&on| on == up)
        .map(|on| sc(&format!("{} --now", verb_of(on))));
    s + &changes(
        enablement.as_deref(),
        &sc(verb),
        both.as_deref(),
        lines(name, up, enabled),
    )
}

fn systemctl(sudo: &str, args: &str, name: &str) -> String {
    format!("{sudo}systemctl --no-ask-password {args} -- {name}")
}

/// `LoadState`, failing the step when systemd does not know the unit and,
/// with `unmasked`, when it is masked.
fn systemd_found(name: &str, unmasked: bool, sudo: &str) -> String {
    let mut s = format!(
        "load=$({}) || exit\n[ \"$load\" != LoadState=not-found ] || {}\n",
        systemctl(sudo, "show --property=LoadState", name),
        fail(&format!("services: {name}: not found by systemd"))
    );
    if unmasked {
        s += &format!(
            "[ \"$load\" != LoadState=masked ] || {}\n",
            fail(&format!("services: {name} is masked"))
        );
    }
    s
}

/// systemd: `restart` starts a stopped unit, so it needs no running check;
/// `reload` runs only on an active one. A masked unit is refused for a
/// restart, as for a start.
fn systemd_action(name: &str, reload: bool, sudo: &str) -> String {
    let sc = |args: &str| systemctl(sudo, args, name);
    let s = systemd_found(name, !reload, sudo);
    if reload {
        s + &format!("{} && up=1\n", sc("is-active --quiet")) + &reload_if_up(name, &sc("reload"))
    } else {
        s + &act(name, false, &sc("restart"))
    }
}

/// OpenRC: `rc-service --exists` first, then `status` and, only when
/// `enabled` is set, the default runlevel, matched on its first field
/// exactly; enablement, then state. `status` exits 32 for a crashed
/// service, which counts as not up; since a plain `start` refuses a
/// crashed service, `zap` resets it to stopped first.
fn openrc(name: &str, up: bool, enabled: Option<bool>, sudo: &str) -> String {
    let mut s = openrc_status(name, sudo);
    if let Some(on) = enabled {
        s += &format!(
            "rl=$({sudo}rc-update show default) || exit\n\
             printf '%s\\n' \"$rl\" | awk -v n={name} '$1 == n {{ f = 1 }} END {{ exit !f }}' \
             && on=1\n"
        );
        s += if on {
            "[ -n \"$on\" ] || e=1\n"
        } else {
            "[ -z \"$on\" ] || e=1\n"
        };
    }
    s += state_differs(up);
    let change_state = if up {
        openrc_start(name, sudo)
    } else {
        rc(sudo, name, "stop")
    };
    let enablement = enabled.map(|on| {
        let verb = if on { "add" } else { "del" };
        format!("{sudo}rc-update {verb} {name} default")
    });
    s + &changes(
        enablement.as_deref(),
        &change_state,
        None,
        lines(name, up, enabled),
    )
}

/// `--exists`, failing the step when OpenRC does not know the service, then
/// `status` into `st`, setting `up` only when it is 0.
fn openrc_status(name: &str, sudo: &str) -> String {
    format!(
        "{sudo}rc-service --exists {name} || {}\n\
         {sudo}rc-service {name} status >/dev/null 2>&1; st=$?\n\
         [ \"$st\" != 0 ] || up=1\n",
        fail(&format!("services: {name}: not found by OpenRC"))
    )
}

/// `start`, after `zap` when `st` is 32 (crashed).
fn openrc_start(name: &str, sudo: &str) -> String {
    format!(
        "if [ \"$st\" = 32 ]; then {} >&2 && {}; else {}; fi",
        rc(sudo, name, "zap"),
        rc(sudo, name, "start"),
        rc(sudo, name, "start")
    )
}

fn rc(sudo: &str, name: &str, verb: &str) -> String {
    format!("{sudo}rc-service {name} {verb}")
}

/// OpenRC: a running service is restarted or reloaded; a stopped or crashed
/// one is started for a restart and left alone for a reload.
fn openrc_action(name: &str, reload: bool, sudo: &str) -> String {
    let rc = |verb: &str| rc(sudo, name, verb);
    let s = openrc_status(name, sudo);
    if reload {
        s + &reload_if_up(name, &rc("reload"))
    } else {
        let command = format!(
            "if [ -n \"$up\" ]; then {}; else {}; fi",
            rc("restart"),
            openrc_start(name, sudo)
        );
        s + &act(name, false, &command)
    }
}

/// launchd, `system` domain: loaded counts as started, and enablement is the
/// disabled override. `enable` comes before `bootstrap`, since a disabled
/// job does not bootstrap; a start that would need it refuses instead.
fn launchd(name: &str, up: bool, enabled: Option<bool>, sudo: &str, plists: &str) -> String {
    let p = quote(&plist(plists, name));
    let (enable_line, state_line) = lines(name, up, enabled);
    let mut s = launchd_found(name, sudo, plists, true);
    if enabled == Some(true) {
        s += &format!(
            "if [ -n \"$off\" ]; then {}; off=; fi\n",
            change(
                &format!("{sudo}launchctl enable system/{name}"),
                std::slice::from_ref(&enable_line)
            )
        );
    }
    if up {
        s += &format!(
            "if [ -z \"$up\" ]; then {}; fi\n",
            launchd_bootstrap(
                name,
                "set enabled = true to start it",
                change(
                    &format!("{sudo}launchctl bootstrap system {p}"),
                    &[state_line]
                )
            )
        );
    } else {
        if enabled == Some(false) {
            s += &format!(
                "if [ -z \"$off\" ]; then {}; fi\n",
                change(
                    &format!("{sudo}launchctl disable system/{name}"),
                    &[enable_line]
                )
            );
        }
        s += &format!(
            "if [ -n \"$up\" ]; then {}; fi\n",
            change(
                &format!("{sudo}launchctl bootout system/{name}"),
                &[state_line]
            )
        );
    }
    s
}

/// The plist of job `name` in `plists`.
fn plist(plists: &str, name: &str) -> String {
    format!("{plists}/{name}.plist")
}

/// `print` into `up` and, with `disabled`, the disabled override into
/// `off`; fails the step when the job is neither loaded nor has a plist in
/// `plists`.
fn launchd_found(name: &str, sudo: &str, plists: &str, disabled: bool) -> String {
    let plist = plist(plists, name);
    let mut s = format!("{sudo}launchctl print system/{name} >/dev/null 2>&1 && up=1\n");
    if disabled {
        // The name's only ERE metacharacter is `.`; the match is the whole line.
        let label = name.replace('.', r"\.");
        s += &format!(
            "dis=$({sudo}launchctl print-disabled system) || exit\n\
             printf '%s\\n' \"$dis\" \
             | grep -Eq '^[[:space:]]*\"{label}\" => (disabled|true)$' && off=1\n"
        );
    }
    s + &format!(
        "[ -n \"$up\" ] || {sudo}test -f {} || {}\n",
        quote(&plist),
        fail(&format!(
            "services: {name}: not found by launchd (no {plist})"
        ))
    )
}

/// A refusal when `off` is set, saying `hint`, then `then`.
fn launchd_bootstrap(name: &str, hint: &str, then: String) -> String {
    format!(
        "[ -z \"$off\" ] || {}; {then}",
        fail(&format!("services: {name} is disabled; {hint}"))
    )
}

/// launchd has no reload: a loaded job is killed and restarted in place by
/// `kickstart -k` for both actions. A job that is not loaded is left alone
/// for a reload and bootstrapped for a restart, refused as a start is when
/// it is disabled.
fn launchd_action(name: &str, reload: bool, sudo: &str, plists: &str) -> String {
    let p = quote(&plist(plists, name));
    let kickstart = format!("{sudo}launchctl kickstart -k system/{name}");
    let s = launchd_found(name, sudo, plists, !reload);
    if reload {
        return s + &reload_if_up(name, &kickstart);
    }
    s + &format!(
        "if [ -n \"$up\" ]; then {}else {}\nfi\n",
        act(name, false, &kickstart),
        launchd_bootstrap(
            name,
            "enable it to restart it",
            act(
                name,
                false,
                &format!("{sudo}launchctl bootstrap system {p}")
            )
            .trim_end()
            .to_owned()
        )
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use super::*;
    use crate::config::ServiceState::{Reloaded, Restarted, Started, Stopped};
    use crate::test_util::temp_dir;

    const STATES: [ServiceState; 2] = [Started, Stopped];
    const ENABLED: [Option<bool>; 3] = [None, Some(true), Some(false)];
    const AGENT: &str = "com.example.agent";

    /// Stand-ins for the init tools and sudo. They log their argv to
    /// `log` and keep the service's state in files: `active` when it runs,
    /// `enabled` (systemd's `is-enabled` word), `on` (in OpenRC's default
    /// runlevel), `off` (launchd's disabled override, in its print format),
    /// `load`, `exists`, `crashed`, `queryfail` and `nosudo`. A change
    /// prints chatter on stdout and stderr, as the real tools do.
    struct Stubs {
        dir: PathBuf,
        bin: PathBuf,
    }

    impl Stubs {
        fn new(tag: &str) -> Stubs {
            let dir = temp_dir(tag);
            let bin = dir.join("bin");
            fs::create_dir(&bin).unwrap();
            let stubs = Stubs { dir, bin };
            let d = stubs.dir.display().to_string();
            stubs.tool(
                "sudo",
                &format!(
                    "echo \"sudo $*\" >> {d}/log\n\
                     [ ! -f {d}/nosudo ] || {{ echo 'sudo: a password is required' >&2; exit 1; }}\n\
                     [ \"$1\" = -n ] || exit 64\nshift; exec \"$@\"\n"
                ),
            );
            stubs.tool(
                "systemctl",
                &format!(
                    "echo \"systemctl $*\" >> {d}/log\n\
                     [ \"$1\" = --no-ask-password ] || exit 64\nshift; verb=$1; shift\n\
                     case $verb in\n\
                     show) echo \"LoadState=$(cat {d}/load)\" ;;\n\
                     is-active) [ -f {d}/active ] ;;\n\
                     is-enabled) if [ -f {d}/queryfail ]; then echo 'Failed to get unit file state: Access denied' >&2; exit 1; fi\n\
                       s=$(cat {d}/enabled); echo \"$s\"\n\
                       case $s in enabled|enabled-runtime|static|indirect|generated|alias|transient) ;; *) exit 1 ;; esac ;;\n\
                     enable) if [ \"$(cat {d}/enabled)\" = masked ]; then echo 'Failed to enable unit: Unit file sshd.service is masked.' >&2; exit 1; fi\n\
                       echo 'Created symlink /etc/systemd/system/multi-user.target.wants/sshd.service.' >&2\n\
                       echo chatter; echo enabled > {d}/enabled; if [ \"$1\" = --now ]; then touch {d}/active; fi ;;\n\
                     disable) runtime=; now=; for a; do case $a in --runtime) runtime=1 ;; --now) now=1 ;; esac; done\n\
                       echo 'Removed \"/etc/systemd/system/multi-user.target.wants/sshd.service\".' >&2; echo chatter\n\
                       case $(cat {d}/enabled),$runtime in enabled-runtime,1|enabled,) echo disabled > {d}/enabled ;; esac\n\
                       if [ -n \"$now\" ]; then rm -f {d}/active; fi ;;\n\
                     start) echo chatter; touch {d}/active ;;\n\
                     stop) echo chatter; rm -f {d}/active ;;\n\
                     restart) echo chatter; touch {d}/active ;;\n\
                     reload) [ -f {d}/active ] || {{ echo 'Job for sshd.service failed: not active' >&2; exit 1; }}\n\
                       echo chatter ;;\n\
                     *) exit 64 ;;\n\
                     esac\n"
                ),
            );
            stubs.tool(
                "rc-service",
                &format!(
                    "echo \"rc-service $*\" >> {d}/log\n\
                     case $1 in\n\
                     --exists) [ -f {d}/exists ] ;;\n\
                     *) case $2 in\n\
                       status) if [ -f {d}/crashed ]; then echo ' * status: crashed'; exit 32; fi\n\
                         [ -f {d}/active ] ;;\n\
                       start) if [ -f {d}/crashed ]; then echo ' * WARNING: sshd has already been started' >&2; exit 1; fi\n\
                         echo ' * Starting sshd ...'; echo ' [ ok ]'; touch {d}/active ;;\n\
                       zap) echo ' * Manually resetting sshd to stopped state'; rm -f {d}/crashed ;;\n\
                       stop) echo ' * Stopping sshd ...'; echo ' [ ok ]'; rm -f {d}/active ;;\n\
                       restart) echo ' * Stopping sshd ...'; echo ' * Starting sshd ...'; touch {d}/active ;;\n\
                       reload) [ -f {d}/active ] || {{ echo ' * ERROR: sshd is not running' >&2; exit 1; }}\n\
                         echo ' * Reloading sshd ...' ;;\n\
                       *) exit 64 ;;\n\
                       esac ;;\n\
                     esac\n"
                ),
            );
            stubs.tool(
                "rc-update",
                &format!(
                    "echo \"rc-update $*\" >> {d}/log\n\
                     case $1 in\n\
                     show) if [ -f {d}/queryfail ]; then echo ' * rc-update: cannot read runlevel' >&2; exit 1; fi\n\
                       echo ' sshd-extra | default'; if [ -f {d}/on ]; then echo ' sshd | default'; fi ;;\n\
                     add) echo ' * service sshd added to runlevel default'; touch {d}/on ;;\n\
                     del) echo ' * service sshd removed from runlevel default'; rm -f {d}/on ;;\n\
                     *) exit 64 ;;\n\
                     esac\n"
                ),
            );
            stubs.tool(
                "launchctl",
                &format!(
                    "echo \"launchctl $*\" >> {d}/log\n\
                     case $1 in\n\
                     print) [ -f {d}/active ] ;;\n\
                     print-disabled) if [ -f {d}/queryfail ]; then echo 'Could not print disabled services' >&2; exit 1; fi\n\
                       echo 'disabled services = {{'\n\
                       echo '\t\"comXexampleXagent\" => disabled'\n\
                       echo '\t\"x{AGENT}\" => disabled'\n\
                       echo '\t\"{AGENT}.helper\" => true'\n\
                       if [ -f {d}/off ]; then echo \"\t\\\"{AGENT}\\\" => $(cat {d}/off)\"\n\
                       else echo '\t\"{AGENT}\" => enabled'; fi\n\
                       echo '}}' ;;\n\
                     enable) echo chatter; rm -f {d}/off ;;\n\
                     disable) echo chatter; echo disabled > {d}/off ;;\n\
                     bootstrap) if [ -f {d}/off ]; then echo 'Bootstrap failed: 5' >&2; exit 5; fi\n\
                       echo chatter; echo 'chatter' >&2; touch {d}/active ;;\n\
                     bootout) echo chatter; rm -f {d}/active ;;\n\
                     kickstart) [ \"$2\" = -k ] && [ -f {d}/active ] || {{ echo 'Could not find service' >&2; exit 113; }}\n\
                       echo chatter ;;\n\
                     *) exit 64 ;;\n\
                     esac\n"
                ),
            );
            stubs
        }

        fn tool(&self, name: &str, body: &str) {
            let path = self.bin.join(name);
            fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }

        fn path(&self, file: &str) -> PathBuf {
            self.dir.join(file)
        }

        fn set(&self, file: &str, content: Option<&str>) {
            match content {
                Some(content) => fs::write(self.path(file), content).unwrap(),
                None => drop(fs::remove_file(self.path(file))),
            }
        }

        fn has(&self, file: &str) -> bool {
            self.path(file).exists()
        }

        fn read(&self, file: &str) -> String {
            fs::read_to_string(self.path(file)).unwrap()
        }

        /// The plist directory, holding `AGENT.plist`.
        fn plists(&self) -> String {
            let plists = self.dir.join("LaunchDaemons");
            fs::create_dir_all(&plists).unwrap();
            fs::write(plists.join(format!("{AGENT}.plist")), "").unwrap();
            plists.display().to_string()
        }

        /// Runs `script` with the stubs first on PATH, after clearing the
        /// log: its exit code, stdout, stderr and the logged argv.
        fn run(&self, script: &str) -> (Option<i32>, String, String, Vec<String>) {
            self.set("log", Some(""));
            let mut command = std::process::Command::new("/bin/sh");
            command.arg("-s").env(
                "PATH",
                format!("{}:{}", self.bin.display(), std::env::var("PATH").unwrap()),
            );
            let out =
                crate::transport::run_with_script(&mut command, script, SERVICE_TIMEOUT).unwrap();
            let log = self.read("log").lines().map(str::to_owned).collect();
            (
                out.code,
                String::from_utf8(out.stdout).unwrap(),
                String::from_utf8(out.stderr).unwrap(),
                log,
            )
        }
    }

    impl Drop for Stubs {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    /// The logged calls that change a service.
    fn mutating(log: &[String]) -> Vec<String> {
        const VERBS: [&str; 12] = [
            "restart",
            "reload",
            "kickstart",
            "enable",
            "disable",
            "start",
            "stop",
            "add",
            "del",
            "bootstrap",
            "bootout",
            "zap",
        ];
        log.iter()
            .filter(|line| !line.starts_with("sudo "))
            .filter(|line| {
                let words: Vec<&str> = line.split(' ').collect();
                let verb = match words[0] {
                    "systemctl" => words[2],
                    "rc-service" => words.get(2).copied().unwrap_or(""),
                    _ => words[1],
                };
                VERBS.contains(&verb)
            })
            .cloned()
            .collect()
    }

    fn systemctl(args: &str) -> String {
        format!("systemctl --no-ask-password {args} -- sshd")
    }

    fn stdout(lines: &[&str], name: &str) -> String {
        if lines.is_empty() {
            return format!("unchanged service {name}\n");
        }
        lines
            .iter()
            .map(|line| format!("{line} service {name}\n"))
            .collect()
    }

    /// Runs `script`, checks it made `calls` and printed `lines`, then reruns
    /// it and checks it is unchanged with no mutating call.
    fn converges(stubs: &Stubs, script: &str, calls: &[String], lines: &[&str], name: &str) {
        let (code, out, err, log) = stubs.run(script);
        assert_eq!(code, Some(0), "{script}\n{err}\n{log:?}");
        assert_eq!(mutating(&log), calls, "{script}");
        assert_eq!(out, stdout(lines, name), "{script}");
        let (code, out, err, log) = stubs.run(script);
        assert_eq!(code, Some(0), "rerun {script}\n{err}");
        assert_eq!(mutating(&log), Vec::<String>::new(), "rerun {script}");
        assert_eq!(out, format!("unchanged service {name}\n"), "rerun");
    }

    /// Runs the action `script` twice: each run exits 0, makes `calls` and
    /// prints `line` for `name`, with the tools' chatter kept off stdout.
    fn acts(stubs: &Stubs, script: &str, calls: &[String], line: &str, name: &str) {
        for run in ["first", "rerun"] {
            let (code, out, err, log) = stubs.run(script);
            assert_eq!(code, Some(0), "{run} {name} {script}\n{err}\n{log:?}");
            assert_eq!(mutating(&log), calls, "{run} {script}");
            assert_eq!(out, format!("{line}\n"), "{run} {script}");
        }
    }

    #[test]
    fn init_for_maps_every_known_distro() {
        for distro in [
            "arch",
            "archarm",
            "manjaro",
            "endeavouros",
            "debian",
            "ubuntu",
            "raspbian",
            "linuxmint",
            "pop",
            "fedora",
            "rhel",
            "centos",
            "rocky",
            "almalinux",
        ] {
            assert_eq!(init_for(distro), Some(Init::Systemd), "{distro}");
        }
        assert_eq!(init_for("alpine"), Some(Init::OpenRc));
        assert_eq!(init_for("macos"), Some(Init::Launchd));
        assert_eq!(init_for("gentoo"), None);
        assert_eq!(init_for(""), None);
        assert_eq!(
            [Init::Systemd, Init::OpenRc, Init::Launchd].map(|init| init.to_string()),
            ["systemd", "OpenRC", "launchd"]
        );
    }

    #[test]
    fn systemd_changes_only_what_differs_and_reruns_unchanged() {
        let stubs = Stubs::new("svc-systemd");
        for state in STATES {
            for enabled in ENABLED {
                for active in [false, true] {
                    for on in [false, true] {
                        stubs.set("load", Some("loaded\n"));
                        stubs.set("active", active.then_some(""));
                        stubs.set("enabled", Some(if on { "enabled\n" } else { "disabled\n" }));
                        let up = state == Started;
                        let e = enabled.is_some_and(|want| want != on);
                        let s = active != up;
                        let (ev, el) = if enabled == Some(true) {
                            ("enable", "enabled")
                        } else {
                            ("disable", "disabled")
                        };
                        let (sv, sl) = if up {
                            ("start", "started")
                        } else {
                            ("stop", "stopped")
                        };
                        let together = e && s && enabled == Some(up);
                        let (calls, lines) = if together {
                            (vec![systemctl(&format!("{ev} --now"))], vec![el, sl])
                        } else {
                            let mut calls = Vec::new();
                            let mut lines = Vec::new();
                            if e {
                                calls.push(systemctl(ev));
                                lines.push(el);
                            }
                            if s {
                                calls.push(systemctl(sv));
                                lines.push(sl);
                            }
                            (calls, lines)
                        };
                        let script = script(Init::Systemd, "sshd", state, enabled, "", "");
                        converges(&stubs, &script, &calls, &lines, "sshd");
                        assert_eq!(stubs.has("active"), up);
                        let now = stubs.read("enabled");
                        let want = enabled.unwrap_or(on);
                        assert_eq!(now.trim(), if want { "enabled" } else { "disabled" });
                    }
                }
            }
        }
    }

    #[test]
    fn systemd_pairs_a_change_in_one_command_and_splits_the_rest() {
        let stubs = Stubs::new("svc-systemd-pairs");
        for (state, enabled, active, on, calls) in [
            (Started, Some(true), false, "disabled", vec!["enable --now"]),
            (Stopped, Some(false), true, "enabled", vec!["disable --now"]),
            (
                Stopped,
                Some(true),
                true,
                "disabled",
                vec!["enable", "stop"],
            ),
            (
                Started,
                Some(false),
                false,
                "enabled",
                vec!["disable", "start"],
            ),
            (Started, Some(true), false, "static", vec!["start"]),
            (Started, Some(true), false, "indirect", vec!["start"]),
            (Started, Some(true), true, "enabled-runtime", vec!["enable"]),
            (Started, Some(true), false, "generated", vec!["start"]),
            (Started, Some(true), true, "alias", vec![]),
            (Stopped, Some(false), false, "static", vec![]),
            (
                Stopped,
                Some(false),
                false,
                "enabled-runtime",
                vec!["disable --runtime"],
            ),
            (
                Stopped,
                Some(false),
                true,
                "enabled-runtime",
                vec!["disable --runtime", "stop"],
            ),
            (
                Started,
                Some(false),
                true,
                "enabled-runtime",
                vec!["disable --runtime"],
            ),
        ] {
            stubs.set("load", Some("loaded\n"));
            stubs.set("active", active.then_some(""));
            stubs.set("enabled", Some(on));
            let script = script(Init::Systemd, "sshd", state, enabled, "", "");
            let (code, out, err, log) = stubs.run(&script);
            assert_eq!(code, Some(0), "{on}: {err}");
            let calls: Vec<String> = calls.into_iter().map(systemctl).collect();
            assert_eq!(mutating(&log), calls, "{state:?} {enabled:?} {on}");
            assert!(!out.contains("chatter"), "{out}");
            let (code, out, _, log) = stubs.run(&script);
            assert_eq!(code, Some(0), "rerun {on}");
            assert_eq!(mutating(&log), Vec::<String>::new(), "rerun {on}");
            assert_eq!(out, "unchanged service sshd\n", "rerun {on}");
        }
        // A masked unit: enabling it is refused before any call, and with
        // enabled = false it is left masked and only stopped.
        stubs.set("load", Some("masked\n"));
        stubs.set("enabled", Some("masked\n"));
        stubs.set("active", None);
        let (code, out, err, log) =
            stubs.run(&script(Init::Systemd, "sshd", Stopped, Some(true), "", ""));
        assert_eq!((code, out.as_str()), (Some(1), ""));
        assert_eq!(err, "services: sshd is masked\n");
        assert_eq!(mutating(&log), Vec::<String>::new());
        stubs.set("active", Some(""));
        converges(
            &stubs,
            &script(Init::Systemd, "sshd", Stopped, Some(false), "", ""),
            &[systemctl("stop")],
            &["stopped"],
            "sshd",
        );
        assert_eq!(stubs.read("enabled"), "masked\n");
        stubs.set("load", Some("loaded\n"));
        // enabled omitted never asks is-enabled.
        stubs.set("active", None);
        let (_, _, _, log) = stubs.run(&script(Init::Systemd, "sshd", Started, None, "", ""));
        assert!(
            !log.iter().any(|line| line.contains("is-enabled")),
            "{log:?}"
        );
    }

    #[test]
    fn systemd_refuses_a_unit_it_cannot_find_or_start() {
        let stubs = Stubs::new("svc-systemd-refuse");
        stubs.set("active", None);
        stubs.set("enabled", Some("disabled"));
        for state in STATES {
            for enabled in ENABLED {
                stubs.set("load", Some("not-found\n"));
                let (code, out, err, log) =
                    stubs.run(&script(Init::Systemd, "sshd", state, enabled, "", ""));
                assert_eq!(code, Some(1));
                assert_eq!(out, "");
                assert_eq!(err, "services: sshd: not found by systemd\n");
                assert_eq!(log.len(), 1, "{log:?}");

                stubs.set("load", Some("masked\n"));
                let (code, out, err, log) =
                    stubs.run(&script(Init::Systemd, "sshd", state, enabled, "", ""));
                if state == Started || enabled == Some(true) {
                    assert_eq!((code, out.as_str()), (Some(1), ""));
                    assert_eq!(err, "services: sshd is masked\n");
                    assert_eq!(log.len(), 1, "{log:?}");
                } else {
                    assert_eq!((code, err.as_str()), (Some(0), ""));
                    assert_eq!(out, "unchanged service sshd\n");
                }
            }
        }
    }

    #[test]
    fn openrc_changes_enablement_then_state_and_reruns_unchanged() {
        let stubs = Stubs::new("svc-openrc");
        stubs.set("exists", Some(""));
        for state in STATES {
            for enabled in ENABLED {
                for active in [false, true] {
                    for on in [false, true] {
                        stubs.set("active", active.then_some(""));
                        stubs.set("on", on.then_some(""));
                        let up = state == Started;
                        let mut calls = Vec::new();
                        let mut lines = Vec::new();
                        match enabled {
                            Some(true) if !on => {
                                calls.push("rc-update add sshd default".to_string());
                                lines.push("enabled");
                            }
                            Some(false) if on => {
                                calls.push("rc-update del sshd default".to_string());
                                lines.push("disabled");
                            }
                            _ => {}
                        }
                        if active != up {
                            let (verb, line) = if up {
                                ("start", "started")
                            } else {
                                ("stop", "stopped")
                            };
                            calls.push(format!("rc-service sshd {verb}"));
                            lines.push(line);
                        }
                        let script = script(Init::OpenRc, "sshd", state, enabled, "", "");
                        converges(&stubs, &script, &calls, &lines, "sshd");
                        assert_eq!(stubs.has("active"), up);
                        assert_eq!(stubs.has("on"), enabled.unwrap_or(on));
                    }
                }
            }
        }
    }

    #[test]
    fn openrc_matches_the_runlevel_entry_exactly_and_refuses_a_missing_service() {
        let stubs = Stubs::new("svc-openrc-refuse");
        stubs.set("exists", Some(""));
        stubs.set("active", Some(""));
        stubs.set("on", None);
        // `sshd-extra | default` is listed; it is not sshd.
        let (_, out, _, log) =
            stubs.run(&script(Init::OpenRc, "sshd", Started, Some(true), "", ""));
        assert_eq!(mutating(&log), ["rc-update add sshd default"]);
        assert_eq!(out, "enabled service sshd\n");

        stubs.set("exists", None);
        for state in STATES {
            for enabled in ENABLED {
                let (code, out, err, log) =
                    stubs.run(&script(Init::OpenRc, "sshd", state, enabled, "", ""));
                assert_eq!((code, out.as_str()), (Some(1), ""));
                assert_eq!(err, "services: sshd: not found by OpenRC\n");
                assert_eq!(log, ["rc-service --exists sshd"]);
            }
        }
    }

    #[test]
    fn launchd_enables_before_bootstrap_and_reruns_unchanged() {
        let stubs = Stubs::new("svc-launchd");
        let plists = stubs.plists();
        let plist = format!("{plists}/{AGENT}.plist");
        let launchctl = |args: &str| format!("launchctl {args}");
        for state in STATES {
            for enabled in ENABLED {
                if state == Started && enabled == Some(false) {
                    continue;
                }
                for loaded in [false, true] {
                    for off in [false, true] {
                        stubs.set("active", loaded.then_some(""));
                        stubs.set("off", off.then_some("disabled"));
                        let script = script(Init::Launchd, AGENT, state, enabled, "", &plists);
                        if state == Started && !loaded && off && enabled.is_none() {
                            let (code, out, err, log) = stubs.run(&script);
                            assert_eq!((code, out.as_str()), (Some(1), ""));
                            assert_eq!(
                                err,
                                format!(
                                    "services: {AGENT} is disabled; set enabled = true to start it\n"
                                )
                            );
                            assert_eq!(mutating(&log), Vec::<String>::new());
                            continue;
                        }
                        let mut calls = Vec::new();
                        let mut lines = Vec::new();
                        if enabled == Some(true) && off {
                            calls.push(launchctl(&format!("enable system/{AGENT}")));
                            lines.push("enabled");
                        }
                        if state == Started && !loaded {
                            calls.push(launchctl(&format!("bootstrap system {plist}")));
                            lines.push("started");
                        }
                        if enabled == Some(false) && !off {
                            calls.push(launchctl(&format!("disable system/{AGENT}")));
                            lines.push("disabled");
                        }
                        if state == Stopped && loaded {
                            calls.push(launchctl(&format!("bootout system/{AGENT}")));
                            lines.push("stopped");
                        }
                        converges(&stubs, &script, &calls, &lines, AGENT);
                        assert_eq!(stubs.has("active"), state == Started);
                        assert_eq!(stubs.has("off"), enabled.map_or(off, |on| !on));
                    }
                }
            }
        }
    }

    #[test]
    fn systemd_restarts_any_unit_and_reloads_only_an_active_one() {
        let stubs = Stubs::new("svc-systemd-actions");
        stubs.set("load", Some("loaded\n"));
        for active in [false, true] {
            stubs.set("active", active.then_some(""));
            let restart = script(Init::Systemd, "sshd", Restarted, None, "", "");
            acts(
                &stubs,
                &restart,
                &[systemctl("restart")],
                "restarted service sshd",
                "sshd",
            );
            assert!(stubs.has("active"));

            stubs.set("active", active.then_some(""));
            let reload = script(Init::Systemd, "sshd", Reloaded, None, "", "");
            if active {
                acts(
                    &stubs,
                    &reload,
                    &[systemctl("reload")],
                    "reloaded service sshd",
                    "sshd",
                );
            } else {
                acts(
                    &stubs,
                    &reload,
                    &[],
                    "service sshd is not running; not reloaded",
                    "sshd",
                );
            }
            assert_eq!(stubs.has("active"), active);
        }

        for state in [Restarted, Reloaded] {
            stubs.set("load", Some("not-found\n"));
            let (code, out, err, log) =
                stubs.run(&script(Init::Systemd, "sshd", state, None, "", ""));
            assert_eq!((code, out.as_str()), (Some(1), ""), "{state:?}");
            assert_eq!(err, "services: sshd: not found by systemd\n");
            assert_eq!(log.len(), 1, "{log:?}");
        }
        // A masked unit cannot restart; it is never running, so a reload
        // leaves it alone.
        stubs.set("load", Some("masked\n"));
        stubs.set("active", None);
        let (code, out, err, log) =
            stubs.run(&script(Init::Systemd, "sshd", Restarted, None, "", ""));
        assert_eq!((code, out.as_str()), (Some(1), ""));
        assert_eq!(err, "services: sshd is masked\n");
        assert_eq!(log.len(), 1, "{log:?}");
        acts(
            &stubs,
            &script(Init::Systemd, "sshd", Reloaded, None, "", ""),
            &[],
            "service sshd is not running; not reloaded",
            "sshd",
        );
    }

    #[test]
    fn openrc_restarts_or_starts_and_reloads_only_a_running_service() {
        let stubs = Stubs::new("svc-openrc-actions");
        stubs.set("exists", Some(""));
        let rc = |verb: &str| format!("rc-service sshd {verb}");
        // (active, crashed, restart calls)
        for (active, crashed, restart) in [
            (true, false, vec![rc("restart")]),
            (false, false, vec![rc("start")]),
            (false, true, vec![rc("zap"), rc("start")]),
        ] {
            stubs.set("active", active.then_some(""));
            stubs.set("crashed", crashed.then_some(""));
            let (code, out, err, log) =
                stubs.run(&script(Init::OpenRc, "sshd", Restarted, None, "", ""));
            assert_eq!(code, Some(0), "{err} {log:?}");
            assert_eq!(mutating(&log), restart, "{active} {crashed}");
            assert_eq!(out, "restarted service sshd\n");
            assert!(stubs.has("active") && !stubs.has("crashed"));
            // Once running, a rerun restarts it.
            acts(
                &stubs,
                &script(Init::OpenRc, "sshd", Restarted, None, "", ""),
                &[rc("restart")],
                "restarted service sshd",
                "sshd",
            );

            stubs.set("active", active.then_some(""));
            stubs.set("crashed", crashed.then_some(""));
            let reload = script(Init::OpenRc, "sshd", Reloaded, None, "", "");
            if active {
                acts(
                    &stubs,
                    &reload,
                    &[rc("reload")],
                    "reloaded service sshd",
                    "sshd",
                );
            } else {
                acts(
                    &stubs,
                    &reload,
                    &[],
                    "service sshd is not running; not reloaded",
                    "sshd",
                );
            }
            assert_eq!(
                (stubs.has("active"), stubs.has("crashed")),
                (active, crashed)
            );
        }

        stubs.set("exists", None);
        for state in [Restarted, Reloaded] {
            let (code, out, err, log) =
                stubs.run(&script(Init::OpenRc, "sshd", state, None, "", ""));
            assert_eq!((code, out.as_str()), (Some(1), ""), "{state:?}");
            assert_eq!(err, "services: sshd: not found by OpenRC\n");
            assert_eq!(log, ["rc-service --exists sshd"]);
        }
    }

    #[test]
    fn launchd_kickstarts_a_loaded_job_and_bootstraps_only_for_a_restart() {
        let stubs = Stubs::new("svc-launchd-actions");
        let plists = stubs.plists();
        let plist = format!("{plists}/{AGENT}.plist");
        let kickstart = format!("launchctl kickstart -k system/{AGENT}");
        stubs.set("off", None);
        for (state, line) in [(Restarted, "restarted"), (Reloaded, "reloaded")] {
            stubs.set("active", Some(""));
            acts(
                &stubs,
                &script(Init::Launchd, AGENT, state, None, "", &plists),
                std::slice::from_ref(&kickstart),
                &format!("{line} service {AGENT}"),
                AGENT,
            );
        }

        stubs.set("active", None);
        let restart = script(Init::Launchd, AGENT, Restarted, None, "", &plists);
        let (code, out, err, log) = stubs.run(&restart);
        assert_eq!(code, Some(0), "{err}");
        assert_eq!(
            mutating(&log),
            [format!("launchctl bootstrap system {plist}")]
        );
        assert_eq!(out, format!("restarted service {AGENT}\n"));
        acts(
            &stubs,
            &restart,
            std::slice::from_ref(&kickstart),
            &format!("restarted service {AGENT}"),
            AGENT,
        );

        let not_running = format!("service {AGENT} is not running; not reloaded");
        for off in [false, true] {
            stubs.set("active", None);
            stubs.set("off", off.then_some("disabled"));
            acts(
                &stubs,
                &script(Init::Launchd, AGENT, Reloaded, None, "", &plists),
                &[],
                &not_running,
                AGENT,
            );
        }
        // A reload never reads the disabled overrides, so a failing read
        // does not fail it.
        stubs.set("queryfail", Some(""));
        for active in [false, true] {
            stubs.set("active", active.then_some(""));
            let (code, _, err, log) =
                stubs.run(&script(Init::Launchd, AGENT, Reloaded, None, "", &plists));
            assert_eq!(code, Some(0), "{err}");
            assert!(
                !log.iter().any(|line| line.contains("print-disabled")),
                "{log:?}"
            );
        }
        stubs.set("queryfail", None);
        stubs.set("active", None);
        // Disabled and not loaded: a restart is refused, as a start is.
        let (code, out, err, log) = stubs.run(&restart);
        assert_eq!((code, out.as_str()), (Some(1), ""));
        assert_eq!(
            err,
            format!("services: {AGENT} is disabled; enable it to restart it\n")
        );
        assert_eq!(mutating(&log), Vec::<String>::new());

        let missing = stubs.dir.join("none").display().to_string();
        stubs.set("off", None);
        for state in [Restarted, Reloaded] {
            let (code, out, err, log) =
                stubs.run(&script(Init::Launchd, AGENT, state, None, "", &missing));
            assert_eq!((code, out.as_str()), (Some(1), ""), "{state:?}");
            assert_eq!(
                err,
                format!("services: {AGENT}: not found by launchd (no {missing}/{AGENT}.plist)\n")
            );
            assert_eq!(mutating(&log), Vec::<String>::new());
        }
    }

    #[test]
    fn a_failed_action_fails_the_step_with_the_tools_status() {
        let stubs = Stubs::new("svc-action-fail");
        stubs.tool(
            "systemctl",
            "case $2 in show) echo LoadState=loaded ;; is-active) exit 0 ;; \
             reload|restart) echo 'Job for sshd.service failed.' >&2; exit 3 ;; \
             *) exit 1 ;; esac\n",
        );
        for state in [Restarted, Reloaded] {
            let (code, out, err, _) =
                stubs.run(&script(Init::Systemd, "sshd", state, None, "", ""));
            assert_eq!((code, out.as_str()), (Some(3), ""), "{state:?}");
            assert_eq!(err, "Job for sshd.service failed.\n");
        }
    }

    #[test]
    fn launchd_reads_both_disabled_formats_and_refuses_what_cannot_last() {
        let stubs = Stubs::new("svc-launchd-refuse");
        let plists = stubs.plists();
        stubs.set("active", None);
        // Older releases print `true` for a disabled job. The other labels
        // differ only where a regex dot would match, or hold the name as a
        // substring (`x{AGENT}`, `{AGENT}.helper`), and do not disable it.
        stubs.set("off", Some("true"));
        let (code, _, err, _) =
            stubs.run(&script(Init::Launchd, AGENT, Started, None, "", &plists));
        assert_eq!(code, Some(1));
        assert!(err.contains("is disabled"), "{err}");
        stubs.set("off", None);
        let (code, out, _, _) =
            stubs.run(&script(Init::Launchd, AGENT, Started, None, "", &plists));
        assert_eq!(
            (code, out.as_str()),
            (Some(0), "started service com.example.agent\n")
        );

        let (code, out, err, log) = stubs.run(&script(
            Init::Launchd,
            AGENT,
            Started,
            Some(false),
            "sudo -n ",
            &plists,
        ));
        assert_eq!((code, out.as_str()), (Some(1), ""));
        assert_eq!(
            err,
            format!("services: {AGENT}: started with enabled = false cannot last on launchd\n")
        );
        assert_eq!(log, Vec::<String>::new());

        let missing = stubs.dir.join("none").display().to_string();
        stubs.set("active", None);
        for state in STATES {
            for enabled in [None, Some(true)] {
                let (code, out, err, log) =
                    stubs.run(&script(Init::Launchd, AGENT, state, enabled, "", &missing));
                assert_eq!((code, out.as_str()), (Some(1), ""));
                assert_eq!(
                    err,
                    format!(
                        "services: {AGENT}: not found by launchd (no {missing}/{AGENT}.plist)\n"
                    )
                );
                assert_eq!(mutating(&log), Vec::<String>::new());
            }
        }
        assert!(
            script(Init::Launchd, "sshd", Started, None, "", LAUNCH_DAEMONS)
                .contains("bootstrap system '/Library/LaunchDaemons/sshd.plist'")
        );
    }

    #[test]
    fn with_sudo_every_call_goes_through_sudo_n_after_a_check() {
        let stubs = Stubs::new("svc-sudo");
        let plists = stubs.plists();
        stubs.set("load", Some("loaded\n"));
        stubs.set("enabled", Some("disabled\n"));
        stubs.set("exists", Some(""));
        for (init, name) in [
            (Init::Systemd, "sshd"),
            (Init::OpenRc, "sshd"),
            (Init::Launchd, AGENT),
        ] {
            for state in STATES {
                stubs.set("off", None);
                stubs.set("on", None);
                stubs.set("active", (state == Stopped).then_some(""));
                let enabled = Some(state == Started);
                let (code, _, err, log) =
                    stubs.run(&script(init, name, state, enabled, "sudo -n ", &plists));
                assert_eq!(code, Some(0), "{init} {err} {log:?}");
                assert_eq!(log[0], "sudo -n true", "{init}");
                assert!(!mutating(&log).is_empty(), "{init} {log:?}");
                for (i, line) in log.iter().enumerate().skip(1) {
                    if !line.starts_with("sudo ") {
                        assert_eq!(log[i - 1], format!("sudo -n {line}"), "{init} {log:?}");
                    }
                }
            }
            for state in [Restarted, Reloaded] {
                stubs.set("active", Some(""));
                let (code, _, err, log) =
                    stubs.run(&script(init, name, state, None, "sudo -n ", &plists));
                assert_eq!(code, Some(0), "{init} {err} {log:?}");
                assert_eq!(log[0], "sudo -n true", "{init}");
                assert_eq!(mutating(&log).len(), 1, "{init} {log:?}");
                for (i, line) in log.iter().enumerate().skip(1) {
                    if !line.starts_with("sudo ") {
                        assert_eq!(log[i - 1], format!("sudo -n {line}"), "{init} {log:?}");
                    }
                }
            }
            stubs.set("nosudo", Some(""));
            let (code, out, err, log) =
                stubs.run(&script(init, name, Started, None, "sudo -n ", &plists));
            assert_eq!((code, out.as_str()), (Some(1), ""), "{init}");
            assert_eq!(err, "sudo: a password is required\n");
            assert_eq!(log, ["sudo -n true"]);
            stubs.set("nosudo", None);
        }
    }

    #[test]
    fn a_script_is_one_brace_group_that_can_follow_a_password_check() {
        let stubs = Stubs::new("svc-group");
        stubs.set("load", Some("loaded\n"));
        stubs.set("active", None);
        let script = script(Init::Systemd, "sshd", Started, None, "", "");
        assert!(
            script.starts_with('{') && script.ends_with("}\n"),
            "{script}"
        );
        let (code, out, _, _) = stubs.run(&format!("true && {script}"));
        assert_eq!((code, out.as_str()), (Some(0), "started service sshd\n"));
        let (code, out, _, log) = stubs.run(&format!("false && {script}"));
        assert_eq!((code, out.as_str(), log.len()), (Some(1), "", 0));
    }

    #[test]
    fn a_failed_change_fails_the_step_with_the_tools_status() {
        let stubs = Stubs::new("svc-fail");
        stubs.set("load", Some("loaded\n"));
        stubs.set("active", None);
        stubs.tool(
            "systemctl",
            "case $2 in show) echo LoadState=loaded ;; \
             start) echo 'Job for sshd.service failed.' >&2; exit 3 ;; \
             *) exit 1 ;; esac\n",
        );
        let (code, out, err, _) = stubs.run(&script(Init::Systemd, "sshd", Started, None, "", ""));
        assert_eq!((code, out.as_str()), (Some(3), ""));
        assert_eq!(err, "Job for sshd.service failed.\n");
    }

    #[test]
    fn a_query_that_fails_fails_the_step() {
        let stubs = Stubs::new("svc-query");
        let plists = stubs.plists();
        stubs.set("load", Some("loaded\n"));
        stubs.set("enabled", Some("enabled\n"));
        stubs.set("exists", Some(""));
        stubs.set("queryfail", Some(""));
        for (init, name, err) in [
            (
                Init::Systemd,
                "sshd",
                "Failed to get unit file state: Access denied\n",
            ),
            (Init::OpenRc, "sshd", " * rc-update: cannot read runlevel\n"),
            (Init::Launchd, AGENT, "Could not print disabled services\n"),
        ] {
            for state in STATES {
                for enabled in [Some(true), Some(false)] {
                    if init == Init::Launchd && state == Started && enabled == Some(false) {
                        continue;
                    }
                    stubs.set("active", None);
                    let (code, out, stderr, log) =
                        stubs.run(&script(init, name, state, enabled, "", &plists));
                    assert_eq!((code, out.as_str()), (Some(1), ""), "{init} {log:?}");
                    assert_eq!(stderr, err, "{init}");
                    assert_eq!(mutating(&log), Vec::<String>::new(), "{init}");
                }
            }
        }
        // launchd asks print-disabled in every case.
        let (code, _, _, _) = stubs.run(&script(Init::Launchd, AGENT, Started, None, "", &plists));
        assert_eq!(code, Some(1));
    }

    #[test]
    fn a_crashed_openrc_service_is_zapped_then_started() {
        let stubs = Stubs::new("svc-crashed");
        stubs.set("exists", Some(""));
        stubs.set("active", None);
        stubs.set("crashed", Some(""));
        let script = script(Init::OpenRc, "sshd", Started, None, "", "");
        converges(
            &stubs,
            &script,
            &[
                "rc-service sshd zap".to_string(),
                "rc-service sshd start".to_string(),
            ],
            &["started"],
            "sshd",
        );
        stubs.set("active", None);
        stubs.set("crashed", Some(""));
        let stop = super::script(Init::OpenRc, "sshd", Stopped, None, "", "");
        let (code, out, _, log) = stubs.run(&stop);
        assert_eq!((code, out.as_str()), (Some(0), "unchanged service sshd\n"));
        assert_eq!(mutating(&log), Vec::<String>::new());
    }
}
