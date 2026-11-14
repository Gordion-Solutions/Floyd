//! Which Rust toolchain Floyd builds and reads coverage with.
//!
//! Floyd needs three unstable compiler flags
//! (`-Zcoverage-options=branch,condition`, `-Zmir-include-spans`, and
//! the MIR dump they annotate), and the `llvm-profdata` / `llvm-cov`
//! binaries of the same toolchain. A nightly compiler accepts the
//! flags as is; a stable or beta compiler (rustup `stable`, or
//! Ferrocene, the qualified compiler, which criticalup installs
//! without a rustup toolchain name) accepts them only with
//! `RUSTC_BOOTSTRAP=1`.
//!
//! A [`ToolchainSpec`] names the toolchain:
//!
//! - `<name>` (the default is `nightly`): a rustup toolchain, run as
//!   `cargo +<name>` / `rustc +<name>`.
//! - `env`: the `cargo` and `rustc` the environment provides, with no
//!   `+<name>` argument: `$CARGO` / `$RUSTC` when set (cargo sets
//!   `CARGO` for its subcommands, so `cargo floyd` uses the cargo that
//!   launched it), otherwise `cargo` / `rustc` on `PATH`. This is the
//!   choice for a criticalup-installed Ferrocene.
//! - `path:<dir>`: `<dir>/cargo` and `<dir>/rustc`.
//!
//! [`Toolchain::detect`] runs `rustc --version --verbose` through the
//! spec and reads the channel from `release:`. When the channel is
//! stable or beta, [`Toolchain::bootstrap`] is set and the
//! instrumented build (only that: never the tests Floyd runs, nor the
//! caller's environment) runs with `RUSTC_BOOTSTRAP=1`; see
//! [`crate::instrument::compile_cargo_project_with_toolchain`].

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// The rustup toolchain Floyd uses when none is selected.
pub const DEFAULT_TOOLCHAIN: &str = "nightly";

/// Environment variable that selects the toolchain when no
/// `--toolchain` option is given; same syntax as [`ToolchainSpec::parse`].
pub const TOOLCHAIN_ENV: &str = "FLOYD_TOOLCHAIN";

/// Environment variable the instrumented `cargo build` passes to the
/// `floyd-rustc-shim` `RUSTC_WRAPPER` to ask it to run the real
/// `rustc` with `RUSTC_BOOTSTRAP=1`. Cargo itself, and the build
/// scripts it runs, never see `RUSTC_BOOTSTRAP` this way.
pub const SHIM_BOOTSTRAP_ENV: &str = "FLOYD_RUSTC_BOOTSTRAP";

/// How to reach a toolchain's `cargo` and `rustc`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolchainSpec {
    /// A rustup toolchain, passed as `+<name>`.
    Rustup(String),
    /// `$CARGO` / `$RUSTC`, else `cargo` / `rustc` on `PATH`; no
    /// `+<name>`.
    Env,
    /// `<dir>/cargo` and `<dir>/rustc`; no `+<name>`.
    Dir(PathBuf),
}

impl Default for ToolchainSpec {
    fn default() -> Self {
        ToolchainSpec::Rustup(DEFAULT_TOOLCHAIN.to_string())
    }
}

impl std::fmt::Display for ToolchainSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolchainSpec::Rustup(name) => f.write_str(name),
            ToolchainSpec::Env => f.write_str("env"),
            ToolchainSpec::Dir(dir) => write!(f, "path:{}", dir.display()),
        }
    }
}

impl ToolchainSpec {
    /// Parse a `--toolchain` / `FLOYD_TOOLCHAIN` value: `env`,
    /// `path:<dir>`, or a rustup toolchain name (a leading `+` is
    /// accepted and dropped).
    pub fn parse(value: &str) -> Result<Self, String> {
        let value = value.trim();
        if value == "env" {
            return Ok(ToolchainSpec::Env);
        }
        if let Some(dir) = value.strip_prefix("path:") {
            if dir.is_empty() {
                return Err("--toolchain=path:<dir> needs a directory".to_string());
            }
            return Ok(ToolchainSpec::Dir(PathBuf::from(dir)));
        }
        let name = value.strip_prefix('+').unwrap_or(value);
        if name.is_empty() || name.contains(char::is_whitespace) || name.starts_with('-') {
            return Err(format!(
                "invalid toolchain {value:?} (expected a rustup toolchain name such as \
                 nightly or stable, `env`, or `path:<dir>`)"
            ));
        }
        Ok(ToolchainSpec::Rustup(name.to_string()))
    }

    /// The spec selected by `cli` (a `--toolchain` value), else by
    /// `env_value` (the `FLOYD_TOOLCHAIN` value), else the default
    /// (`nightly`).
    pub fn select(cli: Option<&str>, env_value: Option<&str>) -> Result<Self, String> {
        match cli.or(env_value.filter(|v| !v.trim().is_empty())) {
            Some(v) => Self::parse(v),
            None => Ok(Self::default()),
        }
    }

    /// The `cargo` program and leading arguments.
    fn cargo_program(&self) -> (OsString, Option<String>) {
        match self {
            ToolchainSpec::Rustup(name) => ("cargo".into(), Some(format!("+{name}"))),
            ToolchainSpec::Env => (
                std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()),
                None,
            ),
            ToolchainSpec::Dir(dir) => (dir.join(exe("cargo")).into_os_string(), None),
        }
    }

    /// The `rustc` program and leading arguments.
    fn rustc_program(&self) -> (OsString, Option<String>) {
        match self {
            ToolchainSpec::Rustup(name) => ("rustc".into(), Some(format!("+{name}"))),
            ToolchainSpec::Env => (
                std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()),
                None,
            ),
            ToolchainSpec::Dir(dir) => (dir.join(exe("rustc")).into_os_string(), None),
        }
    }

    /// A `cargo` command for this toolchain. Outside rustup it carries
    /// `RUSTC` naming this toolchain's `rustc`, so cargo compiles with
    /// the compiler [`Toolchain::detect`] probed.
    pub fn cargo_command(&self) -> Command {
        let (program, plus) = self.cargo_program();
        let mut cmd = Command::new(program);
        if let Some(plus) = plus {
            cmd.arg(plus);
        } else {
            cmd.env("RUSTC", self.rustc_program().0);
        }
        cmd
    }

    /// A `rustc` command for this toolchain (no `RUSTC_BOOTSTRAP`; see
    /// [`Toolchain::rustc_command`]).
    pub fn rustc_command(&self) -> Command {
        let (program, plus) = self.rustc_program();
        let mut cmd = Command::new(program);
        if let Some(plus) = plus {
            cmd.arg(plus);
        }
        cmd
    }

    /// The command that installs the LLVM tools for this toolchain,
    /// for error messages.
    pub fn llvm_tools_hint(&self) -> String {
        match self {
            ToolchainSpec::Rustup(name) => {
                format!("rustup component add llvm-tools-preview --toolchain {name}")
            }
            ToolchainSpec::Env | ToolchainSpec::Dir(_) => {
                "the toolchain's LLVM tools: `rustup component add llvm-tools-preview` for a \
                 rustup toolchain, or the `llvm-tools` package in criticalup.toml (then \
                 `criticalup install`) for Ferrocene"
                    .to_string()
            }
        }
    }
}

/// `name` with the platform's executable suffix.
fn exe(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

/// The release channel of a compiler, from its `release:` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    /// A release (`1.99.1`), Ferrocene included.
    Stable,
    /// `1.100.0-beta.3`.
    Beta,
    /// `1.101.0-nightly`.
    Nightly,
    /// A locally built compiler (`1.101.0-dev`), which accepts `-Z`
    /// flags like nightly.
    Dev,
}

impl Channel {
    /// The channel of a `release:` value: the pre-release suffix
    /// decides (`-nightly`, `-beta.N`, `-dev`); none means stable.
    pub fn of_release(release: &str) -> Channel {
        let suffix = release.split_once('-').map(|(_, s)| s).unwrap_or("");
        if suffix.starts_with("nightly") {
            Channel::Nightly
        } else if suffix.starts_with("beta") {
            Channel::Beta
        } else if suffix.starts_with("dev") {
            Channel::Dev
        } else {
            Channel::Stable
        }
    }

    /// Does a compiler of this channel need `RUSTC_BOOTSTRAP=1` to
    /// accept `-Z` flags? True for stable and beta.
    pub fn needs_bootstrap(self) -> bool {
        matches!(self, Channel::Stable | Channel::Beta)
    }

    /// Lowercase name, as serialized.
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Beta => "beta",
            Channel::Nightly => "nightly",
            Channel::Dev => "dev",
        }
    }
}

/// The fields of `rustc --version --verbose` Floyd records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionInfo {
    /// The first line, e.g. `rustc 1.99.1 (abcdef 2026-10-01)`.
    pub version: String,
    /// The `release:` value, e.g. `1.99.1`.
    pub release: String,
    /// The `host:` value, when printed.
    pub host: Option<String>,
}

impl VersionInfo {
    /// Parse `rustc --version --verbose` output. `None` without a
    /// `rustc ` first line or a `release:` line.
    pub fn parse(text: &str) -> Option<VersionInfo> {
        let mut lines = text.lines();
        let version = lines.next()?.trim();
        if !version.starts_with("rustc ") {
            return None;
        }
        let mut release = None;
        let mut host = None;
        for line in lines {
            if let Some(v) = line.strip_prefix("release:") {
                release = Some(v.trim().to_string());
            } else if let Some(v) = line.strip_prefix("host:") {
                host = Some(v.trim().to_string());
            }
        }
        Some(VersionInfo {
            version: version.to_string(),
            release: release?,
            host,
        })
    }
}

/// A selected toolchain and what [`Toolchain::detect`] found out
/// about it.
#[derive(Debug, Clone)]
pub struct Toolchain {
    /// How the toolchain is reached.
    pub spec: ToolchainSpec,
    /// `rustc --version --verbose` fields; `None` for
    /// [`Toolchain::assumed`].
    pub version: Option<VersionInfo>,
    /// The channel (assumed nightly for [`Toolchain::assumed`]).
    pub channel: Channel,
    /// Whether the instrumented build runs with `RUSTC_BOOTSTRAP=1`.
    pub bootstrap: bool,
    lib_dirs: OnceLock<Vec<PathBuf>>,
}

/// Error from probing a toolchain.
#[derive(Debug, Clone)]
pub struct ToolchainError {
    /// The toolchain probed.
    pub spec: ToolchainSpec,
    /// What went wrong.
    pub message: String,
}

impl std::fmt::Display for ToolchainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "toolchain {}: {}", self.spec, self.message)
    }
}

impl std::error::Error for ToolchainError {}

impl Toolchain {
    /// Probe `spec` with `rustc --version --verbose` and decide
    /// whether the instrumented build needs `RUSTC_BOOTSTRAP=1`.
    pub fn detect(spec: ToolchainSpec) -> Result<Toolchain, ToolchainError> {
        let err = |message: String| ToolchainError {
            spec: spec.clone(),
            message,
        };
        let (program, _) = spec.rustc_program();
        let out = spec
            .rustc_command()
            .arg("--version")
            .arg("--verbose")
            .output()
            .map_err(|e| {
                err(format!(
                    "could not run {} --version --verbose: {e}",
                    Path::new(&program).display()
                ))
            })?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(err(format!(
                "rustc --version --verbose failed ({}): {}",
                out.status,
                stderr.trim()
            )));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let info = VersionInfo::parse(&text).ok_or_else(|| {
            err(format!(
                "could not read the version from rustc --version --verbose:\n{text}"
            ))
        })?;
        Ok(Toolchain::from_version(spec, info))
    }

    /// A toolchain whose version is known (from `rustc -vV` output).
    pub fn from_version(spec: ToolchainSpec, info: VersionInfo) -> Toolchain {
        let channel = Channel::of_release(&info.release);
        Toolchain {
            spec,
            version: Some(info),
            channel,
            bootstrap: channel.needs_bootstrap(),
            lib_dirs: OnceLock::new(),
        }
    }

    /// `spec` taken to be a nightly toolchain, without probing it: no
    /// `RUSTC_BOOTSTRAP`. What the functions that predate toolchain
    /// selection use, with the default spec (`nightly`).
    pub fn assumed(spec: ToolchainSpec) -> Toolchain {
        Toolchain {
            spec,
            version: None,
            channel: Channel::Nightly,
            bootstrap: false,
            lib_dirs: OnceLock::new(),
        }
    }

    /// A `cargo` command for this toolchain ([`ToolchainSpec::cargo_command`]).
    pub fn cargo_command(&self) -> Command {
        self.spec.cargo_command()
    }

    /// A `rustc` command for this toolchain, with `RUSTC_BOOTSTRAP=1`
    /// when [`Toolchain::bootstrap`] is set: for Floyd's own
    /// single-file compilations, which spawn nothing that could
    /// inherit it.
    pub fn rustc_command(&self) -> Command {
        let mut cmd = self.spec.rustc_command();
        if self.bootstrap {
            cmd.env("RUSTC_BOOTSTRAP", "1");
        }
        cmd
    }

    /// The release (`1.99.1`), or `unknown` for an unprobed toolchain.
    pub fn release(&self) -> &str {
        self.version
            .as_ref()
            .map(|v| v.release.as_str())
            .unwrap_or("unknown")
    }

    /// The line cargo-floyd prints when it enables `RUSTC_BOOTSTRAP`,
    /// `None` when it does not.
    pub fn bootstrap_note(&self) -> Option<String> {
        self.bootstrap.then(|| {
            let name = self.spec.to_string();
            let release = self.release();
            let what = if name == release {
                name
            } else {
                format!("{name} {release}")
            };
            format!(
                "toolchain {what} is not nightly; enabling RUSTC_BOOTSTRAP=1 for the \
                 instrumented build only"
            )
        })
    }

    /// One-line description for report headers, e.g. `stable (rustc
    /// 1.99.1 (.. ..), stable channel, RUSTC_BOOTSTRAP=1 for the
    /// instrumented build)`.
    pub fn describe(&self) -> String {
        let version = self
            .version
            .as_ref()
            .map(|v| v.version.as_str())
            .unwrap_or("version not probed");
        let mut s = format!(
            "{} ({version}, {} channel",
            self.spec,
            self.channel.as_str()
        );
        if self.bootstrap {
            s.push_str(", RUSTC_BOOTSTRAP=1 for the instrumented build");
        }
        s.push(')');
        s
    }

    /// The record of this toolchain for reports.
    pub fn summary(&self) -> ToolchainSummary {
        ToolchainSummary {
            name: self.spec.to_string(),
            version: self.version.as_ref().map(|v| v.version.clone()),
            release: self.version.as_ref().map(|v| v.release.clone()),
            host: self.version.as_ref().and_then(|v| v.host.clone()),
            channel: self.channel,
            rustc_bootstrap: self.bootstrap,
        }
    }

    /// `rustc --print <what>` through this toolchain.
    fn print(&self, what: &str) -> Result<PathBuf, String> {
        let out = self
            .spec
            .rustc_command()
            .args(["--print", what])
            .output()
            .map_err(|e| format!("could not exec rustc: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "exit status {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if s.is_empty() {
            return Err("printed nothing".to_string());
        }
        Ok(PathBuf::from(s))
    }

    /// `rustc --print sysroot` through this toolchain.
    pub fn sysroot(&self) -> Result<PathBuf, String> {
        self.print("sysroot")
    }

    /// The toolchain's shared-library directories: the host target
    /// libdir (`rustc --print target-libdir`, which holds
    /// `libstd-*.dylib`/`.so`) and `<sysroot>/lib` (which holds
    /// `librustc_driver-*`). Computed once; empty when `rustc` cannot
    /// be queried.
    pub fn lib_dirs(&self) -> &[PathBuf] {
        self.lib_dirs.get_or_init(|| {
            let mut dirs = Vec::new();
            if let Ok(libdir) = self.print("target-libdir") {
                dirs.push(libdir);
            }
            if let Ok(sysroot) = self.sysroot() {
                dirs.push(sysroot.join("lib"));
            }
            dirs.retain(|d| d.is_dir());
            dirs
        })
    }
}

/// The toolchain record in Floyd's reports (JSON `toolchain`, the
/// text header, JUnit `floyd.toolchain.*` properties): qualification
/// evidence of which compiler produced the instrumentation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolchainSummary {
    /// The selection: a rustup toolchain name, `env` or `path:<dir>`.
    pub name: String,
    /// First line of `rustc --version --verbose`; `null` if not probed.
    pub version: Option<String>,
    /// The `release:` value; `null` if not probed.
    pub release: Option<String>,
    /// The `host:` value; `null` if not probed or not printed.
    pub host: Option<String>,
    /// Release channel.
    pub channel: Channel,
    /// Whether the instrumented build ran with `RUSTC_BOOTSTRAP=1`.
    pub rustc_bootstrap: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    const STABLE: &str = "rustc 1.99.1 (2a4c6f3e1 2026-10-29)\n\
        binary: rustc\n\
        commit-hash: 2a4c6f3e1\n\
        commit-date: 2026-10-29\n\
        host: x86_64-apple-darwin\n\
        release: 1.99.1\n\
        LLVM version: 21.1.2\n";
    const NIGHTLY: &str = "rustc 1.101.0-nightly (8e2b1c0aa 2026-11-02)\n\
        binary: rustc\n\
        host: x86_64-unknown-linux-gnu\n\
        release: 1.101.0-nightly\n";
    const FERROCENE: &str = "rustc 1.79.0 (129f3b996 2024-06-10) (Ferrocene by Ferrous Systems)\n\
        binary: rustc\n\
        host: x86_64-unknown-linux-gnu\n\
        release: 1.79.0\n";

    #[test]
    fn version_verbose_is_parsed() {
        let v = VersionInfo::parse(STABLE).unwrap();
        assert_eq!(v.version, "rustc 1.99.1 (2a4c6f3e1 2026-10-29)");
        assert_eq!(v.release, "1.99.1");
        assert_eq!(v.host.as_deref(), Some("x86_64-apple-darwin"));
        assert!(VersionInfo::parse("cargo 1.99.1\nrelease: 1.99.1\n").is_none());
        assert!(VersionInfo::parse("rustc 1.99.1\nhost: x\n").is_none());
    }

    #[test]
    fn channel_comes_from_the_release_suffix() {
        assert_eq!(Channel::of_release("1.99.1"), Channel::Stable);
        assert_eq!(Channel::of_release("1.100.0-beta.3"), Channel::Beta);
        assert_eq!(Channel::of_release("1.101.0-nightly"), Channel::Nightly);
        assert_eq!(Channel::of_release("1.101.0-dev"), Channel::Dev);
        let ferrocene = VersionInfo::parse(FERROCENE).unwrap();
        assert_eq!(Channel::of_release(&ferrocene.release), Channel::Stable);
    }

    #[test]
    fn bootstrap_only_for_stable_and_beta() {
        let tc = |text: &str| {
            Toolchain::from_version(ToolchainSpec::default(), VersionInfo::parse(text).unwrap())
        };
        assert!(tc(STABLE).bootstrap);
        assert!(tc(FERROCENE).bootstrap);
        assert!(!tc(NIGHTLY).bootstrap);
        assert!(Channel::Beta.needs_bootstrap());
        assert!(!Channel::Dev.needs_bootstrap());
        assert!(!Toolchain::assumed(ToolchainSpec::default()).bootstrap);
    }

    fn env_of(cmd: &Command, key: &str) -> Option<Option<OsString>> {
        cmd.get_envs()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.map(OsString::from))
    }

    #[test]
    fn rustc_command_sets_bootstrap_only_when_needed() {
        let stable = Toolchain::from_version(
            ToolchainSpec::Rustup("stable".into()),
            VersionInfo::parse(STABLE).unwrap(),
        );
        let cmd = stable.rustc_command();
        assert_eq!(env_of(&cmd, "RUSTC_BOOTSTRAP"), Some(Some("1".into())));
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, ["+stable"]);
        let nightly = Toolchain::from_version(
            ToolchainSpec::Rustup("nightly".into()),
            VersionInfo::parse(NIGHTLY).unwrap(),
        );
        assert_eq!(env_of(&nightly.rustc_command(), "RUSTC_BOOTSTRAP"), None);
        // `cargo_command` never carries it: the instrumented build
        // routes it through the shim.
        assert_eq!(env_of(&stable.cargo_command(), "RUSTC_BOOTSTRAP"), None);
    }

    #[test]
    fn specs_parse_and_print() {
        assert_eq!(
            ToolchainSpec::parse("nightly").unwrap(),
            ToolchainSpec::Rustup("nightly".into())
        );
        assert_eq!(
            ToolchainSpec::parse("+1.83.0").unwrap(),
            ToolchainSpec::Rustup("1.83.0".into())
        );
        assert_eq!(ToolchainSpec::parse("env").unwrap(), ToolchainSpec::Env);
        assert_eq!(
            ToolchainSpec::parse("path:/opt/fer/bin").unwrap(),
            ToolchainSpec::Dir("/opt/fer/bin".into())
        );
        assert!(ToolchainSpec::parse("").is_err());
        assert!(ToolchainSpec::parse("path:").is_err());
        assert!(ToolchainSpec::parse("a b").is_err());
        assert_eq!(
            ToolchainSpec::Dir("/x".into()).to_string(),
            "path:/x".to_string()
        );
        assert_eq!(
            ToolchainSpec::select(None, None).unwrap(),
            ToolchainSpec::default()
        );
        assert_eq!(
            ToolchainSpec::select(None, Some("stable")).unwrap(),
            ToolchainSpec::Rustup("stable".into())
        );
        assert_eq!(
            ToolchainSpec::select(Some("env"), Some("stable")).unwrap(),
            ToolchainSpec::Env
        );
        assert_eq!(
            ToolchainSpec::select(None, Some("")).unwrap(),
            ToolchainSpec::default()
        );
    }

    #[test]
    fn commands_outside_rustup_have_no_plus_argument() {
        let dir = ToolchainSpec::Dir("/opt/fer/bin".into());
        let cargo = dir.cargo_command();
        assert_eq!(
            Path::new(cargo.get_program()),
            Path::new("/opt/fer/bin").join(exe("cargo"))
        );
        assert_eq!(cargo.get_args().count(), 0);
        assert_eq!(
            env_of(&cargo, "RUSTC"),
            Some(Some(
                Path::new("/opt/fer/bin")
                    .join(exe("rustc"))
                    .into_os_string()
            ))
        );
        assert_eq!(dir.rustc_command().get_args().count(), 0);
        let rustup = ToolchainSpec::Rustup("stable".into()).cargo_command();
        assert_eq!(env_of(&rustup, "RUSTC"), None);
        assert_eq!(rustup.get_args().collect::<Vec<_>>(), ["+stable"]);
    }

    #[test]
    fn llvm_tools_hint_names_the_toolchain() {
        assert_eq!(
            ToolchainSpec::Rustup("stable".into()).llvm_tools_hint(),
            "rustup component add llvm-tools-preview --toolchain stable"
        );
        assert!(ToolchainSpec::Env.llvm_tools_hint().contains("criticalup"));
    }

    #[test]
    fn summary_and_note_record_the_bootstrap() {
        let stable = Toolchain::from_version(
            ToolchainSpec::Rustup("stable".into()),
            VersionInfo::parse(STABLE).unwrap(),
        );
        assert_eq!(
            stable.bootstrap_note().unwrap(),
            "toolchain stable 1.99.1 is not nightly; enabling RUSTC_BOOTSTRAP=1 for the \
             instrumented build only"
        );
        let pinned = Toolchain::from_version(
            ToolchainSpec::Rustup("1.83.0".into()),
            VersionInfo::parse("rustc 1.83.0 (90b35a623 2024-11-26)\nrelease: 1.83.0\n").unwrap(),
        );
        assert!(pinned
            .bootstrap_note()
            .unwrap()
            .starts_with("toolchain 1.83.0 is not nightly;"));
        let s = serde_json::to_value(stable.summary()).unwrap();
        assert_eq!(s["name"], "stable");
        assert_eq!(s["release"], "1.99.1");
        assert_eq!(s["channel"], "stable");
        assert_eq!(s["rustc_bootstrap"], true);
        assert!(Toolchain::assumed(ToolchainSpec::default())
            .bootstrap_note()
            .is_none());
    }
}
