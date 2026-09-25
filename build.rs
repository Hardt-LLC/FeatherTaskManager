use std::{env, path::PathBuf, process::Command};

/// The SDK is often installed without rc.exe being added to PATH.
fn resource_compiler() -> PathBuf {
    if let Some(path) = env::var_os("RC") {
        return path.into();
    }
    let arch = match env::consts::ARCH {
        "aarch64" => "arm64",
        "x86" => "x86",
        _ => "x64",
    };
    let mut roots = Vec::new();
    if let Some(sdk) = env::var_os("WindowsSdkDir") {
        roots.push(PathBuf::from(sdk));
    }
    for key in ["ProgramFiles(x86)", "ProgramFiles"] {
        if let Some(folder) = env::var_os(key) {
            roots.push(PathBuf::from(folder).join("Windows Kits/10"));
        }
    }
    for root in roots {
        let bin = root.join("bin");
        if let Ok(version) = env::var("WindowsSDKVersion") {
            let candidate = bin
                .join(version.trim_end_matches(['/', '\\']))
                .join(arch)
                .join("rc.exe");
            if candidate.is_file() {
                return candidate;
            }
        }
        if let Ok(entries) = bin.read_dir() {
            let mut versions: Vec<_> = entries
                .flatten()
                .filter_map(|entry| {
                    let version: Option<Vec<u32>> = entry
                        .file_name()
                        .to_str()?
                        .split('.')
                        .map(|p| p.parse().ok())
                        .collect();
                    Some((version?, entry.path()))
                })
                .collect();
            versions.sort_by(|a, b| b.0.cmp(&a.0));
            for (_, version) in versions {
                let candidate = version.join(arch).join("rc.exe");
                if candidate.is_file() {
                    return candidate;
                }
            }
        }
        let candidate = bin.join(arch).join("rc.exe");
        if candidate.is_file() {
            return candidate;
        }
    }
    PathBuf::from("rc.exe")
}

fn main() {
    for path in ["app.manifest", "assets/app.rc", "assets/app.ico"] {
        println!("cargo:rerun-if-changed={path}");
    }
    for key in [
        "RC",
        "WindowsSdkDir",
        "WindowsSDKVersion",
        "ProgramFiles(x86)",
        "ProgramFiles",
    ] {
        println!("cargo:rerun-if-env-changed={key}");
    }
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let manifest = root.join("app.manifest");
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("feather-task-manager.res");
    let compiler = resource_compiler();
    let result = Command::new(&compiler)
        .current_dir(&root)
        .arg("/nologo")
        .arg("/fo")
        .arg(&output)
        .arg("/I")
        .arg(root.join("assets"))
        .arg(root.join("assets/app.rc"))
        .output()
        .unwrap_or_else(|e| panic!("Cannot run Windows SDK resource compiler {}: {e}. Install Windows SDK or set RC to rc.exe.", compiler.display()));
    assert!(
        result.status.success(),
        "Windows resources failed to compile:\n{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    println!("cargo:rustc-link-arg={}", output.display());
    println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg=/MANIFESTINPUT:{}", manifest.display());
}
