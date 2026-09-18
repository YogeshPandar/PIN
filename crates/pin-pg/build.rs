//! builds a scalar c probe against the same server headers as pgrx.
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
    for name in ["PGRX_PG_CONFIG_PATH", "CC", "AR"] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    for name in ["cshim/pin_abi.c", "cshim/pin_abi.h", "cshim/am_fields.def"] {
        println!("cargo:rerun-if-changed={name}");
    }
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
    let object = out.join("pin_abi.o");
    let archive = out.join("libpin_abi.a");
    let cc = env::var_os("CC").unwrap_or_else(|| "cc".into());
    let ar = env::var_os("AR").unwrap_or_else(|| "ar".into());
    let mut compile = Command::new(cc);
    compile
        .arg("-D_GNU_SOURCE")
        .args([
            "-std=c11", "-O2", "-fPIC", "-Wall", "-Wextra", "-Werror", "-I",
        ])
        .arg(server_include)
        .arg("-I")
        .arg(include)
        .args(["-c", "cshim/pin_abi.c", "-o"])
        .arg(&object);
    run(&mut compile)?;
    run(Command::new(ar).arg("crs").arg(&archive).arg(&object))?;
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=pin_abi");
    Ok(())
}
