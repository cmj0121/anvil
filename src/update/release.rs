//! The latest release: which asset this build runs, what the GitHub API
//! says the latest tag is, and how that tag compares with this version.
//!
//! The API is asked, and the assets fetched, with the system `curl`, run
//! from an argv (never a shell) with `-q` first so no `~/.curlrc` applies,
//! https only, without a token, each bounded in size and time. Everything it
//! writes lands in a private [`Stage`] under `$TMPDIR` that is removed again.

use std::cmp::Ordering;
use std::ffi::OsString;
use std::fs::{self, DirBuilder, File};
use std::io::{self, Read};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::Deserialize;

use crate::paths;
use crate::plugin::last_line;
use crate::secret::shown_name;
use crate::transport::{TransportError, collect_capped};

const API: &str = "https://api.github.com";
const REPO: &str = "/repos/mini-poc/anvil/releases/latest";
const DOWNLOAD: &str = "https://github.com/mini-poc/anvil/releases/download/";
/// Most bytes of the API answer read.
const BODY_LIMIT: u64 = 1 << 20;
/// Most bytes of the response headers read.
const HEADERS_LIMIT: u64 = 1 << 20;
/// Most bytes of curl's stderr kept.
const STDERR_LIMIT: usize = 64 << 10;
/// curl's own `--max-time`, and anvil's deadline a little past it.
const MAX_TIME: u64 = 30;
const TIMEOUT: Duration = Duration::from_secs(MAX_TIME + 10);
/// curl's exit status for `--max-filesize` exceeded.
const CURL_FILESIZE: i32 = 63;

/// The release asset triple of this build, or `None` where no asset is built.
pub fn target() -> Option<&'static str> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("aarch64-apple-darwin")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("x86_64-apple-darwin")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        // A glibc build gets the static musl asset too.
        Some("x86_64-unknown-linux-musl")
    } else {
        None
    }
}

/// `X.Y.Z` with an optional `-pre`: digits only, no leading zeros, no `+`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    pre: bool,
}

impl Version {
    pub fn parse(v: &str) -> Option<Version> {
        if v.contains('+') {
            return None;
        }
        let (core, pre) = match v.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (v, None),
        };
        if let Some(pre) = pre
            && (pre.is_empty()
                || !pre
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'))
        {
            return None;
        }
        let mut fields = core.split('.').map(number);
        let version = Version {
            major: fields.next()??,
            minor: fields.next()??,
            patch: fields.next()??,
            pre: pre.is_some(),
        };
        fields.next().is_none().then_some(version)
    }
}

/// One version field: ASCII digits, no leading zero, fitting `u64`.
fn number(field: &str) -> Option<u64> {
    let digits = !field.is_empty() && field.bytes().all(|b| b.is_ascii_digit());
    if !digits || (field.len() > 1 && field.starts_with('0')) {
        return None;
    }
    field.parse().ok()
}

impl Ord for Version {
    /// By the numbers; at equal numbers a prerelease is below its `X.Y.Z`.
    /// Two prereleases of one `X.Y.Z` are equal.
    fn cmp(&self, other: &Self) -> Ordering {
        let key = |v: &Version| (v.major, v.minor, v.patch, !v.pre);
        key(self).cmp(&key(other))
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Where the latest release is asked for and where its assets live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    api: String,
    prefix: String,
    /// Plain `http` is allowed: only the debug-only `--release-url`.
    http: bool,
}

impl Source {
    pub fn github() -> Source {
        Source {
            api: format!("{API}{REPO}"),
            prefix: DOWNLOAD.to_owned(),
            http: false,
        }
    }

    /// `base` in place of the API and download hosts, `http` allowed: the
    /// debug-only `--release-url`.
    #[cfg(any(test, debug_assertions))]
    pub fn at(base: &str) -> Source {
        let base = base.trim_end_matches('/');
        Source {
            api: format!("{base}{REPO}"),
            prefix: format!("{base}/releases/download/"),
            http: true,
        }
    }

    fn protocols(&self) -> &'static str {
        if self.http { "=http,https" } else { "=https" }
    }
}

/// One release asset as the API lists it, its URL checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub size: u64,
    pub url: String,
}

/// The latest release: its `vX.Y.Z` tag and this build's three assets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub tag: String,
    pub version: Version,
    pub tarball: Asset,
    pub sha256: Asset,
    pub minisig: Asset,
}

impl Release {
    /// The version the tag names, without its `v`.
    pub fn number(&self) -> &str {
        &self.tag[1..]
    }
}

/// A private directory under `$TMPDIR`, removed with everything in it when
/// dropped.
pub struct Stage {
    dir: PathBuf,
}

impl Stage {
    /// `anvil-update-<16 hex>` from 8 bytes of `/dev/urandom`, so the name
    /// cannot be guessed ahead; there is no weaker fallback.
    pub fn new() -> io::Result<Stage> {
        const ATTEMPTS: u32 = 16;
        let mut urandom = File::open("/dev/urandom")?;
        let mut last = None;
        for _ in 0..ATTEMPTS {
            let mut bytes = [0u8; 8];
            urandom.read_exact(&mut bytes)?;
            let suffix: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
            let dir = std::env::temp_dir().join(format!("anvil-update-{suffix}"));
            match DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => return Ok(Stage { dir }),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => last = Some(err),
                Err(err) => return Err(err),
            }
        }
        Err(last.unwrap_or_else(|| io::Error::other("no free stage name")))
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// The latest release of `source` with this build's assets for `triple`.
pub fn resolve(source: &Source, triple: &str) -> Result<Release, String> {
    let stage =
        Stage::new().map_err(|err| format!("cannot create a directory in $TMPDIR: {err}"))?;
    let answer = query(source, stage.path())?;
    release_of(&answer, &source.prefix, triple)
}

/// What the API answered.
#[derive(Debug)]
struct Answer {
    status: u16,
    /// The headers of the last response, after any redirects.
    headers: String,
    body: Vec<u8>,
}

/// curl's argv for the API query, writing headers and body into `stage`.
fn query_args(source: &Source, stage: &Path) -> Vec<OsString> {
    let protocols = source.protocols();
    let mut args: Vec<OsString> = [
        "-q",
        "--silent",
        "--show-error",
        "--location",
        "--proto",
        protocols,
        "--proto-redir",
        protocols,
        "--connect-timeout",
        "10",
        "--max-time",
        &MAX_TIME.to_string(),
        "--max-filesize",
        &BODY_LIMIT.to_string(),
        "-H",
        "Accept: application/vnd.github+json",
        "-H",
        "X-GitHub-Api-Version: 2022-11-28",
        "-H",
        &format!("User-Agent: anvil/{}", env!("CARGO_PKG_VERSION")),
        "-w",
        "%{http_code}",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    args.extend(["-D".into(), stage.join("headers").into()]);
    args.extend(["-o".into(), stage.join("body").into()]);
    args.push(source.api.clone().into());
    args
}

/// Runs curl for the API query; its status, last headers and body.
fn query(source: &Source, stage: &Path) -> Result<Answer, String> {
    let stdout = curl(query_args(source, stage), TIMEOUT, over_limit)?;
    let status = String::from_utf8_lossy(&stdout)
        .trim()
        .parse()
        .map_err(|_| "curl printed no HTTP status".to_owned())?;
    let headers = read_capped(&stage.join("headers"), HEADERS_LIMIT)?;
    let body = read_capped(&stage.join("body"), BODY_LIMIT)?.ok_or_else(over_limit)?;
    Ok(Answer {
        status,
        headers: last_headers(&String::from_utf8_lossy(&headers.unwrap_or_default())),
        body,
    })
}

/// Runs curl with `args` (the URL last), stdin null and its output
/// captured, killed after `timeout`; its stdout when it exits 0, else why
/// not, with `too_big` for `--max-filesize` exceeded.
fn curl(
    args: Vec<OsString>,
    timeout: Duration,
    too_big: impl FnOnce() -> String,
) -> Result<Vec<u8>, String> {
    let curl = paths::find_program(Path::new("curl"), std::env::var_os("PATH").as_deref())
        .ok_or("curl not found on PATH")?;
    if let Some(url) = args.last() {
        log::debug!("spawn {} {}", curl.display(), url.to_string_lossy());
    }
    let child = Command::new(&curl)
        .args(args)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("cannot start {}: {err}", curl.display()))?;
    let output =
        collect_capped(child, Some(timeout), [64, STDERR_LIMIT]).map_err(|err| match err {
            TransportError::Timeout { after, .. } => {
                format!("curl timed out after {}s", after.as_secs())
            }
            TransportError::Spawn(reason) => format!("cannot run curl: {reason}"),
            other => format!("curl: {other}"),
        })?;
    match output.code {
        Some(0) => Ok(output.stdout),
        Some(CURL_FILESIZE) => Err(too_big()),
        Some(code) => Err(match last_line(&output.stderr) {
            Some(line) => format!("curl exited {code}: {line}"),
            None => format!("curl exited {code}"),
        }),
        None => Err("curl was killed by a signal".to_owned()),
    }
}

/// How large one downloaded asset may be and how long it may take.
pub struct Limit {
    pub bytes: u64,
    /// `bytes` as messages show it.
    pub text: &'static str,
    /// curl's `--max-time`, in seconds.
    pub seconds: u64,
}

/// The tarball: 64 MiB in 300 s.
pub const TARBALL_LIMIT: Limit = Limit {
    bytes: 64 << 20,
    text: "64 MiB",
    seconds: 300,
};

/// A `.sha256` or `.minisig` file: 4 KiB in 30 s.
pub const SMALL_LIMIT: Limit = Limit {
    bytes: 4 << 10,
    text: "4 KiB",
    seconds: 30,
};

/// curl's argv to fetch `asset` into `file`, failing on an HTTP error and on
/// more bytes than the release lists. curl before 8.4 may not enforce
/// `--max-filesize` on a chunked response; [`download`]'s size check after
/// the transfer still refuses it.
fn download_args(source: &Source, asset: &Asset, limit: &Limit, file: &Path) -> Vec<OsString> {
    let protocols = source.protocols();
    let mut args: Vec<OsString> = [
        "-q",
        "--fail",
        "--silent",
        "--show-error",
        "--location",
        "--proto",
        protocols,
        "--proto-redir",
        protocols,
        "--connect-timeout",
        "10",
        "--max-filesize",
        &asset.size.to_string(),
        "--max-time",
        &limit.seconds.to_string(),
    ]
    .iter()
    .map(OsString::from)
    .collect();
    args.extend(["-o".into(), file.into()]);
    args.push(asset.url.clone().into());
    args
}

/// Downloads `asset` into `stage` under its own name, refusing one the
/// release lists as over `limit` before it starts and one whose size is not
/// the listed size after.
pub fn download(
    source: &Source,
    asset: &Asset,
    limit: &Limit,
    stage: &Path,
) -> Result<PathBuf, String> {
    let name = &asset.name;
    let over = || {
        format!(
            "{name} is larger than the {} bytes the release lists",
            asset.size
        )
    };
    if asset.size > limit.bytes {
        return Err(format!(
            "{name} is {} bytes, over the {} limit",
            asset.size, limit.text
        ));
    }
    let file = stage.join(name);
    let timeout = Duration::from_secs(limit.seconds + 10);
    curl(download_args(source, asset, limit, &file), timeout, over)?;
    let size = fs::metadata(&file)
        .map_err(|err| format!("cannot read the downloaded {name}: {err}"))?
        .len();
    if size != asset.size {
        return Err(format!(
            "{name} is {size} bytes, not the {} the release lists",
            asset.size
        ));
    }
    Ok(file)
}

fn over_limit() -> String {
    "GitHub API answer over 1 MiB".to_owned()
}

/// The bytes of `path`, `None` when it holds more than `limit`; a file curl
/// never created (an empty answer) is empty.
fn read_capped(path: &Path, limit: u64) -> Result<Option<Vec<u8>>, String> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Some(Vec::new())),
        Err(err) => return Err(format!("cannot read {}: {err}", path.display())),
    };
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    Ok((bytes.len() as u64 <= limit).then_some(bytes))
}

/// The last header block of `-D` output: one per response when redirected.
fn last_headers(all: &str) -> String {
    let start = all.rfind("\nHTTP/").map_or(0, |at| at + 1);
    all[start..].to_owned()
}

/// The value of header `name` (any case) in one header block.
fn header<'a>(headers: &'a str, name: &str) -> Option<&'a str> {
    headers.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    size: u64,
    browser_download_url: String,
}

/// The release `answer` describes, with its three assets for `triple`, each
/// URL equal to `prefix` + tag + `/` + name.
fn release_of(answer: &Answer, prefix: &str, triple: &str) -> Result<Release, String> {
    match answer.status {
        200 => {}
        404 => return Err("no release published".to_owned()),
        403 | 429 if header(&answer.headers, "x-ratelimit-remaining") == Some("0") => {
            let mut message =
                "GitHub API rate limit reached (60 requests an hour without a token)".to_owned();
            if let Some(reset) = header(&answer.headers, "x-ratelimit-reset")
                .and_then(|reset| reset.parse::<u64>().ok())
            {
                let day = reset % 86_400;
                message += &format!(
                    "; try again after {:02}:{:02} UTC",
                    day / 3600,
                    day % 3600 / 60
                );
            }
            return Err(message);
        }
        status => return Err(format!("GitHub API answered HTTP {status}")),
    }
    let release: ApiRelease = serde_json::from_slice(&answer.body).map_err(|err| {
        // serde can quote the answer; only the place is kept.
        format!(
            "cannot read the GitHub API answer (line {}, column {})",
            err.line(),
            err.column()
        )
    })?;
    let tag = release.tag_name;
    let version = tag
        .strip_prefix('v')
        .and_then(Version::parse)
        .filter(|version| !version.pre)
        .ok_or_else(|| format!("cannot read release tag {}", shown_name(&tag)))?;
    let asset = |name: String| {
        let found = release
            .assets
            .iter()
            .find(|asset| asset.name == name)
            .ok_or_else(|| format!("release {tag} has no asset {name}"))?;
        let url = format!("{prefix}{tag}/{name}");
        if found.browser_download_url != url {
            return Err(format!("unexpected download URL for {name}"));
        }
        // The size becomes curl's `--max-filesize`, where 0 means no limit.
        if found.size == 0 {
            return Err(format!("release {tag} lists {name} as 0 bytes"));
        }
        Ok(Asset {
            name,
            size: found.size,
            url,
        })
    };
    let name = format!("anvil-{tag}-{triple}.tar.gz");
    let tarball = asset(name.clone())?;
    Ok(Release {
        sha256: asset(format!("{name}.sha256"))?,
        minisig: asset(format!("{name}.minisig"))?,
        tarball,
        version,
        tag,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap_or_else(|| panic!("{text} does not parse"))
    }

    #[test]
    fn versions_are_digits_without_leading_zeros_or_build_metadata() {
        assert_eq!(
            v("1.20.300"),
            Version {
                major: 1,
                minor: 20,
                patch: 300,
                pre: false
            }
        );
        assert_eq!(v("0.0.0"), Version::parse("0.0.0").unwrap());
        assert!(v("0.2.0-rc.1").pre);
        assert!(v("0.2.0-rc-1.a").pre);
        for bad in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "01.2.3",
            "1.02.3",
            "1.2.03",
            "00.1.1",
            "1.2.3+b",
            "1.2.3-rc+b",
            "1.2.3-",
            "1.2.3-rc_1",
            "+1.2.3",
            "-1.2.3",
            "1.-2.3",
            "1..3",
            "v1.2.3",
            " 1.2.3",
            "1.2.3 ",
            "1.2.x",
            "١.2.3",
            "18446744073709551616.0.0",
        ] {
            assert_eq!(Version::parse(bad), None, "{bad:?}");
        }
        assert_eq!(v("18446744073709551615.0.0").major, u64::MAX);
    }

    #[test]
    fn a_prerelease_is_below_its_release_and_numbers_compare_as_numbers() {
        assert!(v("0.2.0-rc.1") < v("0.2.0"));
        assert!(v("0.2.0-rc.1") > v("0.1.9"));
        assert!(v("0.10.0") > v("0.9.0"));
        assert!(v("1.0.0") > v("0.99.99"));
        assert_eq!(v("0.2.0").cmp(&v("0.2.0")), Ordering::Equal);
        assert_eq!(v("0.2.0-a").cmp(&v("0.2.0-b")), Ordering::Equal);
    }

    #[test]
    fn this_build_has_a_target_and_its_own_version_parses() {
        assert!(target().is_some());
        assert!(Version::parse(env!("CARGO_PKG_VERSION")).is_some());
    }

    /// An argv as text.
    fn text(args: Vec<OsString>) -> Vec<String> {
        args.into_iter()
            .map(|arg| arg.into_string().unwrap())
            .collect()
    }

    /// curl's argv for `source`, as text.
    fn curl_args(source: &Source) -> Vec<String> {
        text(query_args(source, Path::new("/s")))
    }

    /// The argument after `flag`.
    fn after<'a>(args: &'a [String], flag: &str) -> &'a str {
        let at = args.iter().position(|arg| arg == flag).unwrap();
        &args[at + 1]
    }

    #[test]
    fn curl_runs_q_first_https_only_with_bounds_and_no_token() {
        let args = curl_args(&Source::github());
        assert_eq!(args[0], "-q");
        assert_eq!(after(&args, "--proto"), "=https");
        assert_eq!(after(&args, "--proto-redir"), "=https");
        assert_eq!(after(&args, "--connect-timeout"), "10");
        assert_eq!(after(&args, "--max-time"), "30");
        assert_eq!(after(&args, "--max-filesize"), "1048576");
        assert_eq!(after(&args, "-D"), "/s/headers");
        assert_eq!(after(&args, "-o"), "/s/body");
        assert_eq!(
            args.last().unwrap(),
            "https://api.github.com/repos/mini-poc/anvil/releases/latest"
        );
        assert!(!args.iter().any(|arg| arg.contains("Authorization")));
        assert!(
            args.iter()
                .any(|arg| arg == "Accept: application/vnd.github+json")
        );

        let local = curl_args(&Source::at("http://127.0.0.1:9/"));
        assert_eq!(after(&local, "--proto"), "=http,https");
        assert_eq!(after(&local, "--proto-redir"), "=http,https");
        assert_eq!(
            local.last().unwrap(),
            "http://127.0.0.1:9/repos/mini-poc/anvil/releases/latest"
        );
    }

    #[test]
    fn a_download_runs_q_first_fails_on_http_errors_and_is_bounded() {
        let asset = |size, url: &str| Asset {
            name: "a".to_owned(),
            size,
            url: url.to_owned(),
        };
        let args = text(download_args(
            &Source::github(),
            &asset(1234, "https://github.com/x"),
            &TARBALL_LIMIT,
            Path::new("/s/a.tar.gz"),
        ));
        assert_eq!(args[0], "-q");
        assert!(args.iter().any(|arg| arg == "--fail"));
        assert_eq!(after(&args, "--proto"), "=https");
        assert_eq!(after(&args, "--proto-redir"), "=https");
        assert_eq!(after(&args, "--connect-timeout"), "10");
        assert_eq!(after(&args, "--max-filesize"), "1234");
        assert_eq!(after(&args, "--max-time"), "300");
        assert_eq!(after(&args, "-o"), "/s/a.tar.gz");
        assert_eq!(args.last().unwrap(), "https://github.com/x");
        let small = text(download_args(
            &Source::at("http://127.0.0.1:9"),
            &asset(99, "http://127.0.0.1:9/x"),
            &SMALL_LIMIT,
            Path::new("/s/a"),
        ));
        assert_eq!(after(&small, "--proto"), "=http,https");
        assert_eq!(after(&small, "--max-filesize"), "99");
        assert_eq!(after(&small, "--max-time"), "30");
    }

    #[test]
    fn an_asset_listed_over_its_limit_is_refused_before_curl_runs() {
        let asset = Asset {
            name: "anvil-v9.9.9-x.tar.gz".to_owned(),
            size: (64 << 20) + 1,
            url: "http://127.0.0.1:9/x".to_owned(),
        };
        let stage = Stage::new().unwrap();
        let err = download(
            &Source::at("http://127.0.0.1:9"),
            &asset,
            &TARBALL_LIMIT,
            stage.path(),
        )
        .unwrap_err();
        assert_eq!(
            err,
            "anvil-v9.9.9-x.tar.gz is 67108865 bytes, over the 64 MiB limit"
        );
    }

    const PREFIX: &str = "https://github.com/mini-poc/anvil/releases/download/";
    const TRIPLE: &str = "x86_64-apple-darwin";

    fn body(tag: &str, names: &[&str], prefix: &str) -> Vec<u8> {
        let assets: Vec<_> = names
            .iter()
            .map(|name| {
                serde_json::json!({
                    "name": name,
                    "size": 7,
                    "browser_download_url": format!("{prefix}{tag}/{name}"),
                    "content_type": "application/gzip",
                })
            })
            .collect();
        serde_json::to_vec(&serde_json::json!({
            "tag_name": tag,
            "draft": false,
            "assets": assets,
        }))
        .unwrap()
    }

    fn all_assets(tag: &str) -> Vec<String> {
        let tarball = format!("anvil-{tag}-{TRIPLE}.tar.gz");
        vec![
            format!("anvil-{tag}-aarch64-apple-darwin.tar.gz"),
            format!("{tarball}.sha256"),
            format!("{tarball}.minisig"),
            tarball,
        ]
    }

    fn ok(body: Vec<u8>) -> Answer {
        Answer {
            status: 200,
            headers: String::new(),
            body,
        }
    }

    fn release(tag: &str) -> Result<Release, String> {
        let names = all_assets(tag);
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        release_of(&ok(body(tag, &names, PREFIX)), PREFIX, TRIPLE)
    }

    #[test]
    fn a_release_names_this_triples_three_assets_and_their_urls() {
        let found = release("v0.2.0").unwrap();
        assert_eq!(found.tag, "v0.2.0");
        assert_eq!(found.number(), "0.2.0");
        assert_eq!(found.version, v("0.2.0"));
        let name = "anvil-v0.2.0-x86_64-apple-darwin.tar.gz";
        assert_eq!(
            found.tarball,
            Asset {
                name: name.to_owned(),
                size: 7,
                url: format!("{PREFIX}v0.2.0/{name}"),
            }
        );
        assert_eq!(found.sha256.name, format!("{name}.sha256"));
        assert_eq!(found.minisig.name, format!("{name}.minisig"));
    }

    #[test]
    fn a_missing_asset_is_named() {
        for missing in 1..4 {
            let mut names = all_assets("v0.2.0");
            let gone = names.remove(missing);
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            let err = release_of(&ok(body("v0.2.0", &names, PREFIX)), PREFIX, TRIPLE).unwrap_err();
            assert_eq!(err, format!("release v0.2.0 has no asset {gone}"));
        }
        let other = ["anvil-v0.2.0-aarch64-apple-darwin.tar.gz"];
        let err = release_of(&ok(body("v0.2.0", &other, PREFIX)), PREFIX, TRIPLE).unwrap_err();
        assert_eq!(
            err,
            "release v0.2.0 has no asset anvil-v0.2.0-x86_64-apple-darwin.tar.gz"
        );
    }

    #[test]
    fn an_asset_listed_as_empty_is_refused() {
        let names = all_assets("v0.2.0");
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut doc: serde_json::Value =
            serde_json::from_slice(&body("v0.2.0", &names, PREFIX)).unwrap();
        doc["assets"][2]["size"] = 0.into();
        let err = release_of(&ok(serde_json::to_vec(&doc).unwrap()), PREFIX, TRIPLE).unwrap_err();
        assert_eq!(
            err,
            "release v0.2.0 lists anvil-v0.2.0-x86_64-apple-darwin.tar.gz.minisig as 0 bytes"
        );
    }

    #[test]
    fn a_download_url_other_than_the_built_one_is_refused() {
        let names = all_assets("v0.2.0");
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        for other in [
            "https://evil.example/mini-poc/anvil/releases/download/",
            "http://github.com/mini-poc/anvil/releases/download/",
            "https://github.com/mini-poc/anvil/releases/download/x/../",
        ] {
            let err = release_of(&ok(body("v0.2.0", &names, other)), PREFIX, TRIPLE).unwrap_err();
            assert_eq!(
                err, "unexpected download URL for anvil-v0.2.0-x86_64-apple-darwin.tar.gz",
                "{other}"
            );
        }
    }

    #[test]
    fn a_tag_must_be_v_and_a_plain_version() {
        for tag in [
            "0.2.0",
            "v0.2.0-rc.1",
            "v0.2",
            "v0.02.0",
            "v0.2.0+b",
            "latest",
        ] {
            assert_eq!(
                release(tag).unwrap_err(),
                format!("cannot read release tag {tag}")
            );
        }
        let err = release_of(
            &ok(br#"{"tag_name":"v\u001b[31m","assets":[]}"#.to_vec()),
            PREFIX,
            TRIPLE,
        )
        .unwrap_err();
        assert_eq!(err, "cannot read release tag v\\x1b[31m");
    }

    #[test]
    fn a_body_that_is_not_a_release_is_refused_without_quoting_it() {
        for body in [
            &b"not json secret"[..],
            b"{\"tag_name\":\"secret\"}",
            b"[]",
            b"",
        ] {
            let err = release_of(&ok(body.to_vec()), PREFIX, TRIPLE).unwrap_err();
            assert!(
                err.starts_with("cannot read the GitHub API answer (line "),
                "{err}"
            );
            assert!(!err.contains("secret"), "{err}");
        }
    }

    fn status(status: u16, headers: &str) -> String {
        let answer = Answer {
            status,
            headers: headers.to_owned(),
            body: Vec::new(),
        };
        release_of(&answer, PREFIX, TRIPLE).unwrap_err()
    }

    #[test]
    fn statuses_other_than_200_say_what_went_wrong() {
        assert_eq!(status(404, ""), "no release published");
        let limited = "HTTP/2 403\r\nX-RateLimit-Remaining: 0\r\nx-ratelimit-reset: 1700000000\r\n";
        assert_eq!(
            status(403, limited),
            "GitHub API rate limit reached (60 requests an hour without a token); \
             try again after 22:13 UTC"
        );
        assert_eq!(
            status(429, "x-ratelimit-remaining: 0\r\n"),
            "GitHub API rate limit reached (60 requests an hour without a token)"
        );
        assert_eq!(
            status(403, "x-ratelimit-remaining: 12\r\n"),
            "GitHub API answered HTTP 403"
        );
        assert_eq!(status(500, ""), "GitHub API answered HTTP 500");
        assert_eq!(status(301, ""), "GitHub API answered HTTP 301");
    }

    #[test]
    fn only_the_last_redirect_s_headers_count() {
        let all = "HTTP/1.1 302 Found\r\nx-ratelimit-remaining: 0\r\n\r\n\
                   HTTP/1.1 403 Forbidden\r\nx-ratelimit-remaining: 5\r\n\r\n";
        let last = last_headers(all);
        assert!(last.starts_with("HTTP/1.1 403"), "{last}");
        assert_eq!(header(&last, "X-RateLimit-Remaining"), Some("5"));
        assert_eq!(header("HTTP/1.1 200 OK\r\n", "x-ratelimit-remaining"), None);
    }

    #[test]
    fn a_file_over_its_limit_reads_as_none() {
        let stage = Stage::new().unwrap();
        let path = stage.path().join("body");
        fs::write(&path, [b'x'; 9]).unwrap();
        assert_eq!(read_capped(&path, 9).unwrap(), Some(vec![b'x'; 9]));
        assert_eq!(read_capped(&path, 8).unwrap(), None);
        assert_eq!(
            read_capped(&stage.path().join("none"), 8).unwrap(),
            Some(Vec::new())
        );
    }

    #[test]
    fn a_stage_is_private_and_gone_when_dropped() {
        use std::os::unix::fs::PermissionsExt;

        let stage = Stage::new().unwrap();
        let dir = stage.path().to_path_buf();
        let name = dir.file_name().unwrap().to_str().unwrap().to_owned();
        let suffix = name.strip_prefix("anvil-update-").unwrap();
        assert_eq!(suffix.len(), 16, "{name}");
        assert!(
            suffix
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
            "{name}"
        );
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::write(dir.join("body"), b"x").unwrap();
        let other = Stage::new().unwrap();
        assert_ne!(other.path(), dir);
        drop(stage);
        assert!(!dir.exists());
    }
}
