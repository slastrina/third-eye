//! Optional OS integrations (spec 2026-08-02 N5): the `thirdeye` CLI on
//! PATH and a Finder Quick Action — installed and removed ONLY from
//! Settings, touching exactly the named paths, idempotently. Everything is
//! health-as-value: statuses report what is actually on disk, and every
//! failure comes back as data for the pane to render.

use serde::Serialize;
use tauri::{AppHandle, Manager};

/// Where the bundled CLI lives inside the app (Resources/binaries/thirdeye,
/// staged by `make build-tauri`). None in dev runs without a bundle.
pub fn bundled_cli(app: &AppHandle) -> Option<std::path::PathBuf> {
    let path = app
        .path()
        .resource_dir()
        .ok()?
        .join("binaries")
        .join("thirdeye");
    path.exists().then_some(path)
}

fn home() -> Result<std::path::PathBuf, String> {
    std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .map_err(|_| "no HOME in environment".to_string())
}

/// Symlink targets for the CLI, most preferred first.
fn cli_link_candidates(home: &std::path::Path) -> Vec<std::path::PathBuf> {
    vec![
        std::path::PathBuf::from("/usr/local/bin/thirdeye"),
        home.join(".local/bin/thirdeye"),
    ]
}

/// The installed CLI symlink, if any candidate exists.
fn installed_cli(home: &std::path::Path) -> Option<std::path::PathBuf> {
    cli_link_candidates(home).into_iter().find(|p| {
        // symlink_metadata: a dangling symlink still counts as installed
        // (it should be removable/repairable from the pane).
        std::fs::symlink_metadata(p).is_ok()
    })
}

/// The Finder Quick Action bundle path.
fn quick_action_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join("Library/Services/Work here with Third Eye.workflow")
}

/// Pane snapshot.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrationsStatus {
    /// The bundled CLI binary the installs point at (None in unbundled
    /// dev runs — installs then refuse with a clear error).
    pub cli_bundled: Option<String>,
    pub cli_installed: Option<String>,
    pub finder_installed: Option<String>,
    pub error: Option<String>,
}

fn status_with_error(app: &AppHandle, error: Option<String>) -> IntegrationsStatus {
    let home = match home() {
        Ok(home) => home,
        Err(e) => {
            return IntegrationsStatus {
                cli_bundled: None,
                cli_installed: None,
                finder_installed: None,
                error: Some(e),
            }
        }
    };
    IntegrationsStatus {
        cli_bundled: bundled_cli(app).map(|p| p.display().to_string()),
        cli_installed: installed_cli(&home).map(|p| p.display().to_string()),
        finder_installed: quick_action_path(&home)
            .exists()
            .then(|| quick_action_path(&home).display().to_string()),
        error,
    }
}

#[tauri::command]
pub fn integrations_status(app: AppHandle) -> IntegrationsStatus {
    status_with_error(&app, None)
}

/// Install the CLI: symlink the bundled binary onto PATH. `/usr/local/bin`
/// first (works on most setups); falls back to `~/.local/bin` (created)
/// with the PATH hint left to the pane copy.
#[tauri::command]
pub fn install_cli(app: AppHandle) -> IntegrationsStatus {
    let result = (|| -> Result<(), String> {
        let source = bundled_cli(&app).ok_or(
            "the bundled CLI is missing — this build was not produced by `make build-tauri` \
             (dev builds do not carry it)",
        )?;
        let home = home()?;
        let mut last_error = String::new();
        for target in cli_link_candidates(&home) {
            if let Some(parent) = target.parent() {
                if std::fs::create_dir_all(parent).is_err() {
                    continue;
                }
            }
            // Idempotent: replace whatever is there (ours or stale).
            let _ = std::fs::remove_file(&target);
            match std::os::unix::fs::symlink(&source, &target) {
                Ok(()) => {
                    log::info!("integrations: CLI linked at {}", target.display());
                    return Ok(());
                }
                Err(e) => last_error = format!("{}: {e}", target.display()),
            }
        }
        Err(format!("could not link the CLI anywhere ({last_error})"))
    })();
    status_with_error(&app, result.err())
}

/// Remove every CLI symlink the install could have created.
#[tauri::command]
pub fn remove_cli(app: AppHandle) -> IntegrationsStatus {
    let result = (|| -> Result<(), String> {
        let home = home()?;
        for target in cli_link_candidates(&home) {
            if std::fs::symlink_metadata(&target).is_ok() {
                std::fs::remove_file(&target)
                    .map_err(|e| format!("removing {} failed: {e}", target.display()))?;
                log::info!("integrations: CLI link removed ({})", target.display());
            }
        }
        Ok(())
    })();
    status_with_error(&app, result.err())
}

/// Install the Finder Quick Action: a hand-authored Automator `.workflow`
/// whose shell step runs the CLI on the selected folder(s). Right-click a
/// folder in Finder → Quick Actions → "Work here with Third Eye".
#[tauri::command]
pub fn install_finder_action(app: AppHandle) -> IntegrationsStatus {
    let result = (|| -> Result<(), String> {
        let home = home()?;
        // The workflow calls the INSTALLED CLI when present (stable path),
        // else the bundled one directly.
        let cli = installed_cli(&home)
            .or_else(|| bundled_cli(&app))
            .ok_or("install the CLI first (or use a bundled build)")?;
        let bundle = quick_action_path(&home);
        let contents = bundle.join("Contents");
        std::fs::create_dir_all(&contents).map_err(|e| e.to_string())?;
        std::fs::write(contents.join("Info.plist"), quick_action_info_plist())
            .map_err(|e| e.to_string())?;
        std::fs::write(
            contents.join("document.wflow"),
            quick_action_document(&cli.display().to_string()),
        )
        .map_err(|e| e.to_string())?;
        log::info!(
            "integrations: Quick Action installed at {}",
            bundle.display()
        );
        Ok(())
    })();
    status_with_error(&app, result.err())
}

/// Remove the Quick Action bundle (exactly what install created).
#[tauri::command]
pub fn remove_finder_action(app: AppHandle) -> IntegrationsStatus {
    let result = (|| -> Result<(), String> {
        let home = home()?;
        let bundle = quick_action_path(&home);
        if bundle.exists() {
            std::fs::remove_dir_all(&bundle)
                .map_err(|e| format!("removing {} failed: {e}", bundle.display()))?;
            log::info!("integrations: Quick Action removed");
        }
        Ok(())
    })();
    status_with_error(&app, result.err())
}

/// The Quick Action's service registration: appears for folders in Finder.
fn quick_action_info_plist() -> String {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>NSServices</key>
	<array>
		<dict>
			<key>NSBackgroundColorName</key>
			<string>background</string>
			<key>NSIconName</key>
			<string>NSTouchBarFolderTemplate</string>
			<key>NSMenuItem</key>
			<dict>
				<key>default</key>
				<string>Work here with Third Eye</string>
			</dict>
			<key>NSMessage</key>
			<string>runWorkflowAsService</string>
			<key>NSRequiredContext</key>
			<dict>
				<key>NSApplicationIdentifier</key>
				<string>com.apple.finder</string>
			</dict>
			<key>NSSendFileTypes</key>
			<array>
				<string>public.folder</string>
			</array>
		</dict>
	</array>
</dict>
</plist>
"#
    .to_string()
}

/// The workflow document: one Run-Shell-Script action (`/bin/zsh`, input as
/// arguments) invoking the CLI on the first selected folder.
fn quick_action_document(cli: &str) -> String {
    let script = format!("for f in \"$@\"\ndo\n  \"{cli}\" \"$f\"\ndone");
    let escaped = script
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>AMApplicationBuild</key>
	<string>528</string>
	<key>AMApplicationVersion</key>
	<string>2.10</string>
	<key>AMDocumentVersion</key>
	<string>2</string>
	<key>actions</key>
	<array>
		<dict>
			<key>action</key>
			<dict>
				<key>AMAccepts</key>
				<dict>
					<key>Container</key>
					<string>List</string>
					<key>Optional</key>
					<true/>
					<key>Types</key>
					<array>
						<string>com.apple.cocoa.string</string>
					</array>
				</dict>
				<key>AMActionVersion</key>
				<string>2.0.3</string>
				<key>AMParameterProperties</key>
				<dict>
					<key>COMMAND_STRING</key>
					<dict/>
					<key>CheckedForUserDefaultShell</key>
					<dict/>
					<key>inputMethod</key>
					<dict/>
					<key>shell</key>
					<dict/>
					<key>source</key>
					<dict/>
				</dict>
				<key>AMProvides</key>
				<dict>
					<key>Container</key>
					<string>List</string>
					<key>Types</key>
					<array>
						<string>com.apple.cocoa.string</string>
					</array>
				</dict>
				<key>ActionBundlePath</key>
				<string>/System/Library/Automator/Run Shell Script.action</string>
				<key>ActionName</key>
				<string>Run Shell Script</string>
				<key>ActionParameters</key>
				<dict>
					<key>COMMAND_STRING</key>
					<string>{escaped}</string>
					<key>CheckedForUserDefaultShell</key>
					<true/>
					<key>inputMethod</key>
					<integer>1</integer>
					<key>shell</key>
					<string>/bin/zsh</string>
					<key>source</key>
					<string></string>
				</dict>
				<key>BundleIdentifier</key>
				<string>com.apple.RunShellScript</string>
				<key>CFBundleVersion</key>
				<string>2.0.3</string>
				<key>CanShowSelectedItemsWhenRun</key>
				<false/>
				<key>CanShowWhenRun</key>
				<true/>
				<key>Class Name</key>
				<string>RunShellScriptAction</string>
				<key>InputUUID</key>
				<string>6A6E0C5F-27FD-4A63-B6F5-93B0AD0D5271</string>
				<key>Keywords</key>
				<array>
					<string>Shell</string>
				</array>
				<key>OutputUUID</key>
				<string>D25A1E2A-2BBD-4E4B-8B5C-2C3E43D0A1F2</string>
				<key>UUID</key>
				<string>7C4C9E20-1B0C-4E3E-9E44-6F5A2E8B0C11</string>
				<key>isViewVisible</key>
				<integer>1</integer>
			</dict>
		</dict>
	</array>
	<key>connectors</key>
	<dict/>
	<key>workflowMetaData</key>
	<dict>
		<key>applicationBundleIDsByPath</key>
		<dict/>
		<key>applicationPaths</key>
		<array/>
		<key>inputTypeIdentifier</key>
		<string>com.apple.Automator.fileSystemObject.folder</string>
		<key>outputTypeIdentifier</key>
		<string>com.apple.Automator.nothing</string>
		<key>presentationMode</key>
		<integer>15</integer>
		<key>processesInput</key>
		<integer>0</integer>
		<key>serviceInputTypeIdentifier</key>
		<string>com.apple.Automator.fileSystemObject.folder</string>
		<key>serviceOutputTypeIdentifier</key>
		<string>com.apple.Automator.nothing</string>
		<key>serviceProcessesInput</key>
		<integer>0</integer>
		<key>systemImageName</key>
		<string>NSTouchBarFolderTemplate</string>
		<key>useAutomaticInputType</key>
		<integer>0</integer>
		<key>workflowTypeIdentifier</key>
		<string>com.apple.Automator.servicesMenu</string>
	</dict>
</dict>
</plist>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_action_plists_are_valid_xml_and_name_the_cli() {
        let info = quick_action_info_plist();
        assert!(info.contains("Work here with Third Eye"));
        assert!(info.contains("public.folder"));
        let doc = quick_action_document("/usr/local/bin/thirdeye");
        assert!(doc.contains("/usr/local/bin/thirdeye"));
        assert!(doc.contains("com.apple.Automator.servicesMenu"));
        // plutil validates both documents on macOS.
        for (name, body) in [("Info.plist", &info), ("document.wflow", &doc)] {
            let path = std::env::temp_dir().join(format!("te-qa-{}-{name}", std::process::id()));
            std::fs::write(&path, body).unwrap();
            let lint = std::process::Command::new("/usr/bin/plutil")
                .args(["-lint", path.to_str().unwrap()])
                .output()
                .unwrap();
            assert!(
                lint.status.success(),
                "{name} failed plutil: {}",
                String::from_utf8_lossy(&lint.stdout)
            );
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn cli_candidates_prefer_usr_local_then_home() {
        let home = std::path::Path::new("/Users/alex");
        let candidates = cli_link_candidates(home);
        assert_eq!(
            candidates[0],
            std::path::Path::new("/usr/local/bin/thirdeye")
        );
        assert_eq!(
            candidates[1],
            std::path::Path::new("/Users/alex/.local/bin/thirdeye")
        );
    }
}
