//! Build the rs3fs release executable and package it with Inno Setup.

use anyhow::{Context, Result, ensure};
use std::{
    env,
    ffi::OsString,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

fn main() -> Result<()> {
    ensure!(
        cfg!(windows),
        "The rs3fs installer must be built on Windows."
    );

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut args = env::args_os().skip(1);
    let compiler = find_compiler(root, args.next())?;
    ensure!(args.next().is_none(), "Expected at most one ISCC.exe path.");

    let runtime = find_runtime()?;
    println!("Inno Setup: {}", compiler.display());
    println!("Visual C++ runtime: {}", runtime.display());

    let build_args = [
        "build",
        "--release",
        "--locked",
        "--target-dir",
        "target",
        "--bin",
        env!("CARGO_PKG_NAME"),
        "--message-format=human",
    ];
    println!("> cargo {}", build_args.join(" "));
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(root)
        .args(build_args)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("Could not run Cargo")?;
    ensure!(status.success(), "Cargo build failed ({status}).");
    check_x64(&root.join("target/release/rs3fs.exe"))?;

    let mut runtime_define = OsString::from("/DAppRuntime=");
    runtime_define.push(runtime);
    println!("> ISCC.exe assets/rs3fs.iss");
    let status = Command::new(compiler)
        .current_dir(root)
        .arg(runtime_define)
        .arg(root.join("assets/rs3fs.iss"))
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("Could not run ISCC.exe")?;
    ensure!(
        status.success(),
        "Inno Setup compilation failed ({status})."
    );
    Ok(())
}

fn find_compiler(root: &Path, explicit_path: Option<OsString>) -> Result<PathBuf> {
    if let Some(path) = explicit_path {
        let path = env::current_dir()?.join(path);
        ensure!(path.is_file(), "ISCC.exe not found at {}", path.display());
        return Ok(path);
    }

    let search_path = env::var_os("PATH").unwrap_or_default();
    let standard_paths = [
        ("ProgramFiles", "Inno Setup 7"),
        ("ProgramFiles(x86)", "Inno Setup 7"),
        ("LOCALAPPDATA", "Programs/Inno Setup 7"),
        ("LOCALAPPDATA", "Programs/Inno Setup 7 (32-bit)"),
        ("ProgramFiles(x86)", "Inno Setup 6"),
        ("ProgramFiles", "Inno Setup 6"),
        ("LOCALAPPDATA", "Programs/Inno Setup 6"),
    ]
    .into_iter()
    .filter_map(|(base, subdir)| {
        env::var_os(base).map(|dir| PathBuf::from(dir).join(subdir).join("ISCC.exe"))
    });
    env::split_paths(&search_path)
        .map(|dir| dir.join("ISCC.exe"))
        .chain([
            root.join("target/installer-tools/inno-setup-7/ISCC.exe"),
            root.join("target/installer-tools/inno-setup-6/ISCC.exe"),
        ])
        .chain(standard_paths)
        .find(|path| path.is_file())
        .context(
            "Could not find ISCC.exe. Install Inno Setup 7 from https://jrsoftware.org/inno-setup/downloads or pass the full path to ISCC.exe after --.",
        )
}

fn find_runtime() -> Result<PathBuf> {
    let program_files = env::var_os("ProgramFiles(x86)").context("ProgramFiles(x86) is missing")?;
    let vswhere =
        PathBuf::from(program_files).join("Microsoft Visual Studio/Installer/vswhere.exe");
    let result = Command::new(vswhere)
        .args([
            "-all",
            "-products",
            "*",
            "-utf8",
            "-find",
            r"VC\Redist\MSVC\*\x64\Microsoft.VC*.CRT\vcruntime140.dll",
        ])
        .output()
        .context("Could not locate Visual Studio Build Tools with vswhere.exe")?;
    ensure!(
        result.status.success(),
        "vswhere failed ({}): {}",
        result.status,
        String::from_utf8_lossy(&result.stderr).trim()
    );
    String::from_utf8(result.stdout)?
        .lines()
        .map(PathBuf::from)
        .filter_map(|path| {
            let version = path.ancestors().nth(3)?.file_name()?.to_str()?;
            let version = version
                .split('.')
                .map(str::parse::<u32>)
                .collect::<Result<Vec<_>, _>>()
                .ok()?;
            path.is_file().then_some((version, path))
        })
        .max_by(|(left, _), (right, _)| left.cmp(right))
        .map(|(_, path)| path)
        .context("Install the x64 Visual C++ Redistributable files through Visual Studio Installer")
}

fn check_x64(path: &Path) -> Result<()> {
    let mut exe = File::open(path).context("Cargo did not produce target/release/rs3fs.exe")?;
    let mut offset = [0; 4];
    exe.seek(SeekFrom::Start(0x3c))?;
    exe.read_exact(&mut offset)?;
    exe.seek(SeekFrom::Start(u32::from_le_bytes(offset).into()))?;
    let mut header = [0; 6];
    exe.read_exact(&mut header)?;
    ensure!(
        header == *b"PE\0\0\x64\x86",
        "The installer requires an x64 rs3fs.exe."
    );
    Ok(())
}
