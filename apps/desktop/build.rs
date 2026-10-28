use std::{env, fs, path::PathBuf, process::Command};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=assets/icons/cshell.ico");
    println!("cargo:rerun-if-changed=assets/icons/cshell.rc");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return Ok(());
    }
    let root =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").ok_or("CARGO_MANIFEST_DIR is missing")?);
    let out = PathBuf::from(env::var_os("OUT_DIR").ok_or("OUT_DIR is missing")?);
    if env::var("TARGET").unwrap_or_default().ends_with("-gnu") {
        let file = out.join("cshell-icons.o");
        let status = Command::new("windres")
            .current_dir(&root)
            .args(["assets/icons/cshell.rc", "-O", "coff", "-o"])
            .arg(&file)
            .status()?;
        if !status.success() {
            return Err("CShell icon resource compilation with windres failed".into());
        }
        println!("cargo:rustc-link-arg-bins={}", file.display());
        return Ok(());
    }
    let mut candidates = vec![];
    if let Some(rc) = env::var_os("RC") {
        candidates.push(PathBuf::from(rc));
    }
    let base = env::var_os("ProgramFiles(x86)")
        .map(PathBuf::from)
        .unwrap_or_else(|| "C:/Program Files (x86)".into())
        .join("Windows Kits/10/bin");
    if let Ok(versions) = fs::read_dir(base) {
        let mut versions = versions.flatten().map(|p| p.path()).collect::<Vec<_>>();
        versions.sort();
        versions.reverse();
        for version in versions {
            candidates.push(version.join("x64/rc.exe"));
        }
    }
    let rc = candidates
        .into_iter()
        .find(|p| p.is_file())
        .unwrap_or_else(|| "rc.exe".into());
    let file = out.join("cshell-icons.res");
    let status = Command::new(rc)
        .current_dir(&root)
        .arg("/nologo")
        .arg(format!("/fo{}", file.display()))
        .arg("assets/icons/cshell.rc")
        .status()?;
    if !status.success() {
        return Err("CShell icon resource compilation with Windows SDK failed".into());
    }
    println!("cargo:rustc-link-arg-bins={}", file.display());
    Ok(())
}
