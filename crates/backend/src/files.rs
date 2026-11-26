//! Safely opening project files (refusing to execute runnable files outside the sandbox)
//! and external HTTPS links.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::state::App;

/// Editors offered in "Open with", if installed.
pub const EDITORS: &[&str] = &[
    "Visual Studio Code",
    "Cursor",
    "Zed",
    "Sublime Text",
    "BBEdit",
    "Nova",
    "Xcode",
    "TextEdit",
];

/// Kinds of file whose default app runs them: opening one would run the agent's code
/// outside the sandbox, so they only open in an editor or Finder.
const RUNS_CODE: &[&str] = &[
    "app", "command", "sh", "bash", "zsh", "fish", "csh", "ksh", "tool", "terminal", "term", "scpt",
    "scptd", "applescript", "osa", "workflow", "action", "shortcut", "pkg", "mpkg", "dmg", "iso",
    "img", "cdr", "sparseimage", "sparsebundle", "jar", "jnlp", "py", "pyw", "rb", "pl", "php",
    "url", "webloc", "inetloc", "fileloc", "ftploc", "mailloc", "afploc", "vncloc", "news", "osax",
    "prefpane", "kext", "plugin", "bundle", "dylib", "so", "mobileconfig", "appex", "xpc", "saver",
    "qlgenerator", "mdimporter", "wdgt", "xcodeproj", "xcworkspace", "html", "htm", "svg",
];

fn installed_app(name: &str) -> bool {
    let home = std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Applications"));
    ["/Applications".into(), "/System/Applications".into()]
        .into_iter()
        .chain(home)
        .any(|dir: PathBuf| dir.join(format!("{name}.app")).exists())
}

/// Whether opening `path` in its default app could run it.
pub fn runs_code(path: &Path) -> bool {
    let extension = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if RUNS_CODE.contains(&extension.as_str()) {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if path.is_file() && std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0) {
            return true;
        }
    }
    false
}

#[derive(Debug, Clone, Serialize)]
pub struct FileInfo {
    /// Its full path.
    pub path: String,
    /// Its default app might run it; offer an editor or Finder instead.
    pub runs_code: bool,
    pub is_dir: bool,
}

/// A project file the chat mentions, if it exists in the project's folders.
pub fn project_file(app: &App, project: &str, path: &str) -> Option<FileInfo> {
    let found = app.project(project)?.find(path)?;
    Some(FileInfo {
        runs_code: runs_code(&found),
        is_dir: found.is_dir(),
        path: found.display().to_string(),
    })
}

/// The editors installed, for "Open with".
pub fn editors() -> Vec<&'static str> {
    EDITORS.iter().copied().filter(|name| installed_app(name)).collect()
}

/// Opens a project file in its default app (`how` = `default`), in one of [`EDITORS`]
/// (`editor`, with `editor` set), or shows it in Finder (`finder`).
pub fn open_file(
    app: &App,
    project_id: &str,
    path: &str,
    how: &str,
    editor: Option<&str>,
) -> anyhow::Result<()> {
    let project = app
        .project(project_id)
        .ok_or_else(|| anyhow::anyhow!("that project no longer exists"))?;
    let file = project
        .find(path)
        .ok_or_else(|| anyhow::anyhow!("{path} isn't in this project's folders"))?;
    let mut command = std::process::Command::new("/usr/bin/open");
    match how {
        "finder" => {
            command.arg("-R");
        }
        "editor" => {
            let editor = editor
                .filter(|e| EDITORS.contains(e) && installed_app(e))
                .ok_or_else(|| anyhow::anyhow!("that editor isn't installed"))?;
            command.args(["-a", editor]);
        }
        _ if runs_code(&file) => {
            anyhow::bail!(
                "{} could run as a program, outside the sandbox. Open it in an editor or show it in Finder.",
                file.display()
            );
        }
        _ => {}
    }
    let status = command.arg(&file).status()?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("couldn't open {}", file.display())
    }
}

/// Opens an https link in the default browser.
pub fn open_url(url: &str) -> std::io::Result<()> {
    if !url.starts_with("https://") {
        return Err(std::io::Error::other("only https links"));
    }
    let opener = if cfg!(target_os = "macos") {
        "/usr/bin/open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener).arg(url).status().map(|_| ())
}
