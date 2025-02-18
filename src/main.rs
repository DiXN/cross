#![deny(missing_debug_implementations)]

extern crate atty;
#[macro_use]
extern crate error_chain;
#[macro_use]
extern crate lazy_static;
extern crate libc;
extern crate rustc_version;
extern crate semver;
extern crate toml;

#[cfg(not(target_os = "windows"))]
extern crate nix;

#[cfg(target_os = "windows")]
extern crate winapi;

mod cargo;
mod cli;
mod docker;
mod errors;
mod extensions;
mod file;
mod id;
mod interpreter;
mod rustc;
mod rustup;

use std::io::Write;
use std::process::ExitStatus;
use std::{env, io, process};

use toml::{Value, value::Table};

use cargo::{Root, Subcommand};
use errors::*;
use rustc::{TargetList, VersionMetaExt};

#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Host {
    Other,

    // OSX
    X86_64AppleDarwin,

    // Linux
    X86_64UnknownLinuxGnu,

    // Windows MSVC
    X86_64PcWindowsMsvc
}

impl Host {
    /// Checks if this `(host, target)` pair is supported by `cross`
    ///
    /// `target == None` means `target == host`
    fn is_supported(&self, target: Option<&Target>) -> bool {
        if *self == Host::X86_64AppleDarwin {
            target.map(|t| t.is_apple() || t.needs_docker()).unwrap_or(false)
        } else if *self == Host::X86_64UnknownLinuxGnu {
            target.map(|t| t.needs_docker()).unwrap_or(true)
        } else if *self == Host::X86_64PcWindowsMsvc {
            target.map(|t| t.needs_docker()).unwrap_or(false)
        } else {
            false
        }
    }

    fn triple(&self) -> &'static str {
        match *self {
            Host::X86_64AppleDarwin => "x86_64-apple-darwin",
            Host::X86_64UnknownLinuxGnu => "x86_64-unknown-linux-gnu",
            Host::X86_64PcWindowsMsvc => "x86_64-pc-windows-msvc",
            Host::Other => unimplemented!()
        }
    }
}

impl<'a> From<&'a str> for Host {
    fn from(s: &str) -> Host {
        match s {
            "x86_64-apple-darwin" => Host::X86_64AppleDarwin,
            "x86_64-unknown-linux-gnu" => Host::X86_64UnknownLinuxGnu,
            "x86_64-pc-windows-msvc" => Host::X86_64PcWindowsMsvc,
            _ => Host::Other,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    BuiltIn { triple: String },
    Custom { triple: String },
}

impl Target {
    fn new_built_in(triple: &str) -> Self {
        Target::BuiltIn { triple: triple.to_owned() }
    }

    fn new_custom(triple: &str) -> Self {
        Target::Custom { triple: triple.to_owned() }
    }

    fn triple(&self) -> &str {
        match *self {
            Target::BuiltIn{ref triple} => triple,
            Target::Custom{ref triple} => triple,
        }
    }

    fn is_apple(&self) -> bool {
        self.triple().contains("apple")
    }

    fn is_bare_metal(&self) -> bool {
        self.triple().contains("thumb")
    }

    fn is_builtin(&self) -> bool {
        match *self {
            Target::BuiltIn{ .. } => true,
            Target::Custom{ .. } => false,
        }
    }

    fn is_bsd(&self) -> bool {
        self.triple().contains("bsd") || self.triple().contains("dragonfly")
    }

    fn is_solaris(&self) -> bool {
        self.triple().contains("solaris")
    }

    fn is_android(&self) -> bool {
        self.triple().contains("android")
    }

    fn is_emscripten(&self) -> bool {
        self.triple().contains("emscripten")
    }

    fn is_linux(&self) -> bool {
        self.triple().contains("linux") && !self.is_android()
    }

    fn is_windows(&self) -> bool {
        self.triple().contains("windows")
    }

    fn needs_docker(&self) -> bool {
        self.is_linux() || self.is_android() || self.is_bare_metal() || self.is_bsd() ||
        self.is_solaris() || !self.is_builtin() || self.is_windows() || self.is_emscripten()
    }

    fn needs_interpreter(&self) -> bool {
        let native = self.triple().starts_with("x86_64") ||
            self.triple().starts_with("i586") ||
            self.triple().starts_with("i686");

        !native && (self.is_linux() || self.is_windows() || self.is_bare_metal())
    }
}

impl Target {
    fn from(triple: &str, target_list: &TargetList) -> Target {
        if target_list.contains(triple) {
            Target::new_built_in(triple)
        } else {
            Target::new_custom(triple)
        }
    }
}

impl From<Host> for Target {
    fn from(host: Host) -> Target {
        match host {
            Host::X86_64UnknownLinuxGnu => Target::new_built_in("x86_64-unknown-linux-gnu"),
            Host::X86_64AppleDarwin => Target::new_built_in("x86_64-apple-darwin"),
            Host::X86_64PcWindowsMsvc => Target::new_built_in("x86_64-pc-windows-msvc"),
            Host::Other => unimplemented!(),
        }
    }
}

pub fn main() {
    fn show_backtrace() -> bool {
        env::var("RUST_BACKTRACE").as_ref().map(|s| &s[..]) == Ok("1")
    }

    match run() {
        Err(e) => {
            let stderr = io::stderr();
            let mut stderr = stderr.lock();

            writeln!(stderr, "error: {}", e).ok();

            for e in e.iter().skip(1) {
                writeln!(stderr, "caused by: {}", e).ok();
            }

            if show_backtrace() {
                if let Some(backtrace) = e.backtrace() {
                    writeln!(stderr, "{:?}", backtrace).ok();
                }
            } else {
                writeln!(stderr,
                         "note: run with `RUST_BACKTRACE=1` for a backtrace")
                    .ok();
            }

            process::exit(1)
        }
        Ok(status) => {
            if !status.success() {
                process::exit(status.code().unwrap_or(1))
            }
        }
    }
}

fn run() -> Result<ExitStatus> {
    let target_list = rustc::target_list(false)?;
    let args = cli::parse(&target_list);

    if args.all.iter().any(|a| a == "--version" || a == "-V") &&
       args.subcommand.is_none() {
        println!(concat!("cross ", env!("CARGO_PKG_VERSION"), "{}"),
                 include_str!(concat!(env!("OUT_DIR"), "/commit-info.txt")));
    }

    let verbose =
        args.all.iter().any(|a| a == "--verbose" || a == "-v" || a == "-vv");

    let version_meta = rustc_version::version_meta().chain_err(|| "couldn't fetch the `rustc` version")?;
    if let Some(root) = cargo::root()? {
        let host = version_meta.host();

        if host.is_supported(args.target.as_ref()) {
            let target = args.target
                .unwrap_or(Target::from(host.triple(), &target_list));
            let toml = toml(&root)?;

            let sysroot = rustc::sysroot(&host, &target, verbose)?;
            let toolchain = sysroot.file_name().and_then(|file_name| file_name.to_str())
                .ok_or("couldn't get toolchain name")?;

            let installed_toolchains = rustup::installed_toolchains(verbose)?;

            if !installed_toolchains.into_iter().any(|t| t == toolchain) {
              rustup::install_toolchain(&toolchain, verbose)?;
            }

            let available_targets = rustup::available_targets(&toolchain, verbose)?;
            let uses_xargo = !target.is_builtin() ||
                !available_targets.contains(&target) ||
                if let Some(toml) = toml.as_ref() {
                    toml.xargo(&target)?
                } else {
                    None
                }
                .unwrap_or(false);

            if !uses_xargo && !available_targets.is_installed(&target) {
                rustup::install(&target, &toolchain, verbose)?;
            } else if !rustup::component_is_installed("rust-src", toolchain, verbose)? {
                rustup::install_component("rust-src", toolchain, verbose)?;
            }

            if args.subcommand.map(|sc| sc == Subcommand::Clippy).unwrap_or(false) {
                if !rustup::component_is_installed("clippy", toolchain, verbose)? {
                    rustup::install_component("clippy", toolchain, verbose)?;
                }
            }

            let needs_interpreter = args.subcommand.map(|sc| sc.needs_interpreter()).unwrap_or(false);

            let image_exists = match docker::image(toml.as_ref(), &target) {
                Ok(_) => true,
                Err(err) => {
                    eprintln!("Warning: {} Falling back to `cargo` on the host.", err);
                    false
                },
            };

            if image_exists && target.needs_docker() &&
               args.subcommand.map(|sc| sc.needs_docker()).unwrap_or(false) {
                if version_meta.needs_interpreter() &&
                    needs_interpreter &&
                    target.needs_interpreter() &&
                    !interpreter::is_registered(&target)? {
                        docker::register(&target, verbose)?
                }

                return docker::run(&target,
                                   &args.all,
                                   &root,
                                   toml.as_ref(),
                                   uses_xargo,
                                   &sysroot,
                                   verbose);
            }
        }
    }

    cargo::run(&args.all, verbose)
}


/// Parsed `Cross.toml`
#[derive(Debug)]
pub struct Toml {
    table: Table,
}

impl Toml {
    /// Returns the `target.{}.image` part of `Cross.toml`
    pub fn image(&self, target: &Target) -> Result<Option<&str>> {
        let triple = target.triple();

        if let Some(value) = self.table.get("target").and_then(|t| t.get(triple)).and_then(|t| t.get("image")) {
            Ok(Some(value.as_str()
                .ok_or_else(|| {
                    format!("target.{}.image must be a string", triple)
                })?))
        } else {
            Ok(None)
        }
    }

    /// Returns the `target.{}.runner` part of `Cross.toml`
    pub fn runner(&self, target: &Target) -> Result<Option<String>> {
        let triple = target.triple();

        if let Some(value) = self.table.get("target").and_then(|t| t.get(triple)).and_then(|t| t.get("runner")) {
            let value = value.as_str()
                .ok_or_else(|| format!("target.{}.runner must be a string", triple))?
                .to_string();
            Ok(Some(value))
        } else {
            Ok(None)
        }
    }

    /// Returns the `build.image` or the `target.{}.xargo` part of `Cross.toml`
    pub fn xargo(&self, target: &Target) -> Result<Option<bool>> {
        let triple = target.triple();

        if let Some(value) = self.table.get("build").and_then(|b| b.get("xargo")) {
            return Ok(Some(value.as_bool()
                .ok_or_else(|| "build.xargo must be a boolean")?));
        }

        if let Some(value) = self.table.get("target").and_then(|b| b.get(triple)).and_then(|t| t.get("xargo")) {
            Ok(Some(value.as_bool()
                .ok_or_else(|| {
                    format!("target.{}.xargo must be a boolean", triple)
                })?))
        } else {
            Ok(None)
        }
    }

    /// Returns the list of environment variables to pass through for `target`,
    /// including variables specified under `build` and under `target`.
    pub fn env_passthrough(&self, target: &Target) -> Result<Vec<&str>> {
        let mut bwl = self.build_env_passthrough()?;
        let mut twl = self.target_env_passthrough(target)?;
        bwl.extend(twl.drain(..));

        Ok(bwl)
    }

    /// Returns the `build.env.passthrough` part of `Cross.toml`
    fn build_env_passthrough(&self) -> Result<Vec<&str>> {
        match self.table.get("build").and_then(|b| b.get("env")).and_then(|e| e.get("passthrough")) {
            Some(&Value::Array(ref vec)) => {
                if vec.iter().any(|val| val.as_str().is_none()) {
                    bail!("every build.env.passthrough element must be a string");
                }
                Ok(vec.iter().map(|val| val.as_str().unwrap()).collect())
            },
            _ => Ok(Vec::new()),
        }
    }

    /// Returns the `target.<triple>.env.passthrough` part of `Cross.toml` for `target`.
    fn target_env_passthrough(&self, target: &Target) -> Result<Vec<&str>> {
        let triple = target.triple();

        let key = format!("target.{}.env.passthrough", triple);

        match self.table.get("target").and_then(|t| t.get(triple)).and_then(|t| t.get("env")).and_then(|e| e.get("passthrough")) {
            Some(&Value::Array(ref vec)) => {
                if vec.iter().any(|val| val.as_str().is_none()) {
                    bail!("every {} element must be a string", key);
                }
                Ok(vec.iter().map(|val| val.as_str().unwrap()).collect())
            },
            _ => Ok(Vec::new()),
        }
    }
}

/// Parses the `Cross.toml` at the root of the Cargo project (if any)
fn toml(root: &Root) -> Result<Option<Toml>> {
    let path = root.path().join("Cross.toml");

    if path.exists() {
        Ok(Some(Toml {
            table: if let Ok(Value::Table(table)) = file::read(&path)?.parse() {
                table
            } else {
                return Err(format!("couldn't parse {} as TOML table", path.display()).into())
            },
        }))
    } else {
        Ok(None)
    }
}
