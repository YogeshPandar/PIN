//! builds audited C adapters against the same server headers as pgrx.
//! no downloads, rust installation, or cross-compilation occur here.
//! ownership and command contracts: docs/api-evidence.md, build01.
#![forbid(unsafe_code)]

use std::env;
use std::error::Error;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

fn output(program: &OsStr, args: &[&str]) -> Result<String, Box<dyn Error>> {
    let result = Command::new(program).args(args).output()?;
    if !result.status.success() {
        return Err(format!(
            "{program:?} failed: {}",
            String::from_utf8_lossy(&result.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(result.stdout)?.trim().to_owned())
}

fn run(command: &mut Command) -> Result<(), Box<dyn Error>> {
    let result = command.output()?;
    if !result.status.success() {
        return Err(format!(
            "{command:?} failed: {}",
            String::from_utf8_lossy(&result.stderr)
        )
        .into());
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    for name in [
        "PGRX_PG_CONFIG_PATH",
        "CC",
        "AR",
        "CARGO_FEATURE_TEST_HOOKS",
        "PIN_RELEASE_PACKAGE",
        "PIN_BUILD_REVISION",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    for name in [
        "cshim/pin_abi.c",
        "cshim/pin_abi.h",
        "cshim/am_fields.def",
        "cshim/pin_storage.c",
        "cshim/pin_storage.h",
        "cshim/pin_count.c",
        "cshim/pin_count.h",
        "cshim/pin_parallel.c",
        "cshim/pin_parallel.h",
        "cshim/pin_grouped.c",
        "cshim/pin_grouped.h",
        "cshim/pin_primary_sort.c",
        "cshim/pin_primary_sort.h",
    ] {
        println!("cargo:rerun-if-changed={name}");
    }
    // deployment builds must carry a commit and exclude error injection.
    let deployment = env::var_os("PIN_RELEASE_PACKAGE");
    if deployment.is_some() && deployment.as_deref() != Some(OsStr::new("1")) {
        return Err("PIN_RELEASE_PACKAGE must be unset or 1".into());
    }
    let revision = env::var("PIN_BUILD_REVISION").unwrap_or_else(|_| "unrecorded".into());
    let valid_revision = revision.len() == 40
        && revision
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase());
    if deployment.is_some()
        && (!valid_revision || env::var_os("CARGO_FEATURE_TEST_HOOKS").is_some())
    {
        return Err("deployment builds require a source commit and forbid test-hooks".into());
    }
    if revision != "unrecorded" && !valid_revision {
        return Err("PIN_BUILD_REVISION must be a lowercase 40-digit commit".into());
    }
    println!("cargo:rustc-env=PIN_BUILD_REVISION={revision}");
    let host = env::var("HOST")?;
    let target = env::var("TARGET")?;
    if host != target || target != "x86_64-unknown-linux-gnu" {
        return Err("G0 requires a native x86_64-unknown-linux-gnu build".into());
    }
    let pg_config = env::var_os("PGRX_PG_CONFIG_PATH")
        .ok_or("set PGRX_PG_CONFIG_PATH to the PostgreSQL 18.6 pg_config used by pgrx")?;
    if !Path::new(&pg_config).is_absolute() {
        return Err("PGRX_PG_CONFIG_PATH must be an absolute path".into());
    }
    let version = output(&pg_config, &["--version"])?;
    if version.split_whitespace().nth(1) != Some("18.6") {
        return Err(format!("G0 requires PostgreSQL 18.6, found {version}").into());
    }
    let server_include = output(&pg_config, &["--includedir-server"])?;
    let include = output(&pg_config, &["--includedir"])?;
    println!("cargo:rerun-if-changed={}", Path::new(&pg_config).display());
    println!("cargo:rerun-if-changed={server_include}");
    let out = PathBuf::from(env::var_os("OUT_DIR").ok_or("OUT_DIR is missing")?);
    let mut objects = Vec::new();
    let archive = out.join("libpin_abi.a");
    let cc = env::var_os("CC").unwrap_or_else(|| "cc".into());
    let ar = env::var_os("AR").unwrap_or_else(|| "ar".into());
    for source in [
        "pin_abi",
        "pin_storage",
        "pin_count",
        "pin_parallel",
        "pin_grouped",
        "pin_primary_sort",
    ] {
        let object = out.join(format!("{source}.o"));
        let mut compile = Command::new(&cc);
        if env::var_os("CARGO_FEATURE_TEST_HOOKS").is_some() {
            compile.arg("-DPIN_TEST_HOOKS");
        }
        // postgres headers require its aliasing, overflow and precision semantics.
        compile
            .arg("-D_GNU_SOURCE")
            .args([
                "-std=c11",
                "-O2",
                "-fPIC",
                "-fno-strict-aliasing",
                "-fwrapv",
                "-fexcess-precision=standard",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-isystem",
            ])
            .arg(&server_include)
            .arg("-isystem")
            .arg(&include)
            .arg("-c")
            .arg(format!("cshim/{source}.c"))
            .arg("-o")
            .arg(&object);
        run(&mut compile)?;
        objects.push(object);
    }
    run(Command::new(ar).arg("crs").arg(&archive).args(&objects))?;
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=pin_abi");
    Ok(())
}
