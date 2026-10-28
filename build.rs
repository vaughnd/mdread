//! Embeds the Windows resources (icon, version information, application manifest) into
//! mdread.exe. Does nothing for other targets.

use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // Build scripts run on the host, so check the target rather than cfg!(windows).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let icon = root.join("assets/icons/mdread.ico");
    let manifest = root.join("assets/windows/mdread.exe.manifest");
    for p in [&icon, &manifest] {
        println!("cargo:rerun-if-changed={}", p.display());
    }

    let env = |k: &str| std::env::var(k).unwrap_or_default();
    let version = env("CARGO_PKG_VERSION");
    // FILEVERSION wants four numbers; pre-release tags only go into the strings.
    let numeric = format!("{},{},{},0", env("CARGO_PKG_VERSION_MAJOR"), env("CARGO_PKG_VERSION_MINOR"), env("CARGO_PKG_VERSION_PATCH"));
    let rc_path = |p: &Path| p.display().to_string().replace('\\', "\\\\");
    let rc = format!(
        r#"#pragma code_page(65001)
1 ICON "{icon}"
1 24 "{manifest}"
1 VERSIONINFO
FILEVERSION {numeric}
PRODUCTVERSION {numeric}
FILEFLAGSMASK 0x3F
FILEFLAGS 0x0
FILEOS 0x40004
FILETYPE 0x1
FILESUBTYPE 0x0
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "FileDescription", "mdread Markdown viewer"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "mdread"
      VALUE "LegalCopyright", "Copyright (c) 2026 Vaughn Dickson and mdread contributors (MIT License)"
      VALUE "OriginalFilename", "mdread.exe"
      VALUE "ProductName", "mdread"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
        icon = rc_path(&icon),
        manifest = rc_path(&manifest),
    );
    let out = Path::new(&env("OUT_DIR")).join("mdread.rc");
    std::fs::write(&out, rc).expect("write mdread.rc");
    match embed_resource::compile(&out, embed_resource::NONE) {
        embed_resource::CompilationResult::NotAttempted(why) => {
            println!("cargo:warning=mdread.exe built without icon, version info and manifest: {why}");
        }
        r => r.manifest_optional().unwrap(),
    }
}
