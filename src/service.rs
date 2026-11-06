use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(target_os = "linux")]
use std::process::Output;

use anyhow::{Context, Result, bail, ensure};

/// Resolve `rat`'s private data directory without creating it.
///
/// # Errors
///
/// Returns an error when configured paths are invalid or the current directory cannot
/// be determined.
#[must_use = "use the resolved rat data directory"]
pub fn data_root() -> Result<PathBuf> {
    if let Some(value) = env::var_os("RAT_HOME") {
        let path = PathBuf::from(value);
        ensure!(!path.as_os_str().is_empty(), "RAT_HOME must not be empty");
        return make_absolute(path);
    }

    if let Some(value) = env::var_os("XDG_DATA_HOME") {
        let path = PathBuf::from(value);
        ensure!(path.is_absolute(), "XDG_DATA_HOME must be an absolute path");
        return make_absolute(path.join("rat"));
    }

    let home = env::var_os("HOME")
        .context("HOME is not set; set RAT_HOME to choose rat's data directory")?;
    make_absolute(PathBuf::from(home).join(".local").join("share").join("rat"))
}

/// Install rat's login service only after the caller has obtained explicit consent.
///
/// # Errors
///
/// Returns an error when the platform service manager is unavailable or installation fails.
pub fn install(root: &Path) -> Result<()> {
    let root = make_absolute(root.to_path_buf())?;
    let executable = env::current_exe().context("cannot determine the rat executable path")?;

    #[cfg(target_os = "macos")]
    {
        install_launch_agent(&root, &executable)
    }
    #[cfg(target_os = "linux")]
    {
        install_systemd_user_service(&root, &executable)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (root, executable);
        bail!("rat service installation is supported only on macOS and Linux")
    }
}

/// Remove rat's login service only when it belongs to the selected data directory.
///
/// # Errors
///
/// Returns an error when the service belongs to another data directory or cannot be removed.
pub fn uninstall(root: &Path) -> Result<()> {
    let root = make_absolute(root.to_path_buf())?;

    #[cfg(target_os = "macos")]
    {
        uninstall_launch_agent(&root)
    }
    #[cfg(target_os = "linux")]
    {
        uninstall_systemd_user_service(&root)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = root;
        bail!("rat service management is supported only on macOS and Linux")
    }
}

/// Return whether rat's service for this data directory is currently available.
///
/// # Errors
///
/// Returns an error when the current directory cannot be determined.
#[must_use = "check whether the configured rat service is active"]
pub fn installed_for_root(root: &Path) -> Result<bool> {
    let root = make_absolute(root.to_path_buf())?;

    #[cfg(target_os = "macos")]
    {
        if launch_agent_matches(&root)? {
            launch_agent_loaded()
        } else {
            Ok(false)
        }
    }
    #[cfg(target_os = "linux")]
    {
        let path = systemd_user_unit_path()?;
        if systemd_unit_matches(&root)? && systemd_user_unit_loaded(&path)? {
            systemd_user_service_active()
        } else {
            Ok(false)
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = root;
        Ok(false)
    }
}

/// Describe the current user's `rat` service without changing host configuration.
///
/// # Errors
///
/// Returns an error when the service manager is unavailable or cannot report its state.
#[must_use = "report the service-management result"]
pub fn status(root: &Path) -> Result<String> {
    let root = make_absolute(root.to_path_buf())?;

    #[cfg(target_os = "macos")]
    {
        let path = launch_agent_path()?;
        if !launch_agent_matches(&root)? {
            return Ok(format!("not installed for data root {}", root.display()));
        }
        let state = if launch_agent_loaded()? {
            "loaded"
        } else {
            "configured but not loaded"
        };
        Ok(format!(
            "{state} for data root {} ({})",
            root.display(),
            path.display()
        ))
    }
    #[cfg(target_os = "linux")]
    {
        require_systemd_user()?;
        let path = systemd_user_unit_path()?;
        if !systemd_unit_matches(&root)? {
            return Ok(format!("not installed for data root {}", root.display()));
        }
        if !systemd_user_unit_loaded(&path)? {
            return Ok(format!(
                "configured for data root {} but not loaded from its user unit file",
                root.display()
            ));
        }
        let enabled = systemctl_succeeds(["is-enabled", "--quiet", "rat.service"])?;
        let active = systemd_user_service_active()?;
        let state = match (enabled, active) {
            (true, true) => "enabled and active",
            (true, false) => "enabled but inactive",
            (false, true) => "active but not enabled",
            (false, false) => "configured but not enabled or active",
        };
        Ok(format!(
            "{state} for data root {} ({})",
            root.display(),
            path.display()
        ))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = root;
        bail!("rat service management is supported only on macOS and Linux")
    }
}

fn make_absolute(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(env::current_dir()?.join(path))
    }
}

fn home_dir() -> Result<PathBuf> {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set; cannot locate the user's service configuration")?;
    make_absolute(home)
}

fn write_private_file(path: &Path, content: &str) -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("cannot write service file {}", path.display()))?;
    file.write_all(content.as_bytes())
        .with_context(|| format!("cannot write service file {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).with_context(|| {
        format!(
            "cannot restrict service file permissions {}",
            path.display()
        )
    })
}

#[cfg(target_os = "macos")]
const LAUNCH_AGENT_LABEL: &str = "io.github.smartcrabai.rat";

#[cfg(target_os = "macos")]
fn launch_agent_path() -> Result<PathBuf> {
    Ok(home_dir()?
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LAUNCH_AGENT_LABEL}.plist")))
}

#[cfg(target_os = "macos")]
fn launch_agent_domain() -> String {
    format!("gui/{}", unsafe { libc::geteuid() })
}

#[cfg(target_os = "macos")]
fn launch_agent_contents(root: &Path, executable: &Path) -> Result<String> {
    let root = xml_text(path_text(root)?)?;
    let executable = xml_text(path_text(executable)?)?;
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n\t<key>Label</key>\n\t<string>{LAUNCH_AGENT_LABEL}</string>\n\t<key>ProgramArguments</key>\n\t<array>\n\t\t<string>{executable}</string>\n\t\t<string>daemon</string>\n\t</array>\n\t<key>EnvironmentVariables</key>\n\t<dict>\n\t\t<key>RAT_HOME</key>\n\t\t<string>{root}</string>\n\t</dict>\n\t<key>RunAtLoad</key>\n\t<true/>\n\t<key>KeepAlive</key>\n\t<true/>\n\t<key>AbandonProcessGroup</key>\n\t<true/>\n</dict>\n</plist>\n"
    ))
}

#[cfg(target_os = "macos")]
fn launch_agent_matches(root: &Path) -> Result<bool> {
    let path = launch_agent_path()?;
    let Ok(contents) = fs::read_to_string(path) else {
        return Ok(false);
    };
    let root = xml_text(path_text(root)?)?;
    Ok(
        contents.contains(&format!("<string>{LAUNCH_AGENT_LABEL}</string>"))
            && contents.contains(&format!("<key>RAT_HOME</key>\n\t\t<string>{root}</string>"))
            && contents.contains("<key>ProgramArguments</key>")
            && contents.contains("<string>daemon</string>"),
    )
}

#[cfg(target_os = "macos")]
fn launch_agent_loaded() -> Result<bool> {
    let target = format!("{}/{}", launch_agent_domain(), LAUNCH_AGENT_LABEL);
    Ok(Command::new("launchctl")
        .args(["print", target.as_str()])
        .output()
        .context("cannot run launchctl; rat service status is unavailable")?
        .status
        .success())
}

#[cfg(target_os = "macos")]
fn install_launch_agent(root: &Path, executable: &Path) -> Result<()> {
    let path = launch_agent_path()?;
    fs::create_dir_all(path.parent().context("LaunchAgents path has no parent")?)
        .context("cannot create the user's LaunchAgents directory")?;
    if path.exists() && !launch_agent_matches(root)? {
        bail!(
            "a different rat LaunchAgent is already configured at {}; select its data root before uninstalling it",
            path.display()
        );
    }

    let target = format!("{}/{}", launch_agent_domain(), LAUNCH_AGENT_LABEL);
    if launch_agent_loaded()? {
        run_launchctl(["bootout", target.as_str()])?;
    }
    let contents = launch_agent_contents(root, executable)?;
    write_private_file(&path, &contents)?;
    bootstrap_launch_agent(&launch_agent_domain(), &path)
        .with_context(|| format!("could not load rat's LaunchAgent at {}", path.display()))
}

#[cfg(target_os = "macos")]
fn uninstall_launch_agent(root: &Path) -> Result<()> {
    let path = launch_agent_path()?;
    ensure!(
        launch_agent_matches(root)?,
        "no rat LaunchAgent is installed for data root {}; refusing to alter another service",
        root.display()
    );
    let target = format!("{}/{}", launch_agent_domain(), LAUNCH_AGENT_LABEL);
    if launch_agent_loaded()? {
        run_launchctl(["bootout", target.as_str()])?;
    }
    fs::remove_file(&path).with_context(|| format!("cannot remove {}", path.display()))
}
#[cfg(target_os = "macos")]
fn bootstrap_launch_agent(domain: &str, path: &Path) -> Result<()> {
    let status = Command::new("launchctl")
        .arg("bootstrap")
        .arg(domain)
        .arg(path)
        .status()
        .context("cannot run launchctl")?;
    ensure!(status.success(), "launchctl bootstrap failed with {status}");
    Ok(())
}

#[cfg(target_os = "macos")]
fn run_launchctl<const N: usize>(arguments: [&str; N]) -> Result<()> {
    let status = Command::new("launchctl")
        .args(arguments)
        .status()
        .context("cannot run launchctl")?;
    ensure!(
        status.success(),
        "launchctl {arguments:?} failed with {status}"
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn xml_text(value: &str) -> Result<String> {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '\"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            '\r' => escaped.push_str("&#xD;"),
            '\t' | '\n' => escaped.push(character),
            control if control.is_control() => {
                bail!("path contains a character that XML cannot represent")
            }
            _ => escaped.push(character),
        }
    }
    Ok(escaped)
}
fn path_text(path: &Path) -> Result<&str> {
    path.to_str().context("service paths must be valid UTF-8")
}

#[cfg(target_os = "linux")]
fn systemd_user_unit_path() -> Result<PathBuf> {
    systemd_user_unit_path_from(env::var_os("XDG_CONFIG_HOME").map(PathBuf::from), home_dir)
}

#[cfg(target_os = "linux")]
fn systemd_user_unit_path_from(
    config_home: Option<PathBuf>,
    home_directory: impl FnOnce() -> Result<PathBuf>,
) -> Result<PathBuf> {
    let config_home = match config_home {
        Some(path) => {
            ensure!(
                path.is_absolute(),
                "XDG_CONFIG_HOME must be an absolute path"
            );
            path
        }
        None => home_directory()?.join(".config"),
    };
    Ok(config_home.join("systemd").join("user").join("rat.service"))
}

#[cfg(target_os = "linux")]
fn systemd_unit_contents(root: &Path, executable: &Path) -> Result<String> {
    let executable = systemd_quote(path_text(executable)?);
    let root = systemd_quote(&format!("RAT_HOME={}", path_text(root)?));
    Ok(format!(
        "[Unit]\nDescription=rat user command scheduler\n\n[Service]\nType=simple\nExecStart={executable} daemon\nEnvironment={root}\nEnvironment=\"RAT_SERVICE_ID=io.github.smartcrabai.rat\"\nKillMode=process\n\n[Install]\nWantedBy=default.target\n"
    ))
}

#[cfg(target_os = "linux")]
fn systemd_unit_matches(root: &Path) -> Result<bool> {
    let path = systemd_user_unit_path()?;
    let Ok(contents) = fs::read_to_string(path) else {
        return Ok(false);
    };
    let root = systemd_quote(&format!("RAT_HOME={}", path_text(root)?));
    Ok(contents.contains("Description=rat user command scheduler")
        && contents.contains(&format!("Environment={root}"))
        && contents.contains("Environment=\"RAT_SERVICE_ID=io.github.smartcrabai.rat\"")
        && contents.contains("ExecStart=")
        && contents.contains(" daemon\n"))
}

#[cfg(target_os = "linux")]
fn require_systemd_user() -> Result<()> {
    let available = Command::new("systemctl")
        .args(["--user", "show-environment"])
        .output()
        .context("cannot run systemctl; start the scheduler manually with `rat daemon`")?;
    ensure!(
        available.status.success(),
        "systemd --user is unavailable; start the scheduler manually with `rat daemon`"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn systemctl_output(arguments: &[&str]) -> Result<Output> {
    Command::new("systemctl")
        .arg("--user")
        .args(arguments)
        .output()
        .context("cannot run systemctl; start the scheduler manually with `rat daemon`")
}

#[cfg(target_os = "linux")]
fn systemctl_succeeds<const N: usize>(arguments: [&str; N]) -> Result<bool> {
    Ok(systemctl_output(&arguments)?.status.success())
}

#[cfg(target_os = "linux")]
fn systemd_user_fragment() -> Result<Option<PathBuf>> {
    let output = systemctl_output(&["show", "rat.service", "--property=FragmentPath", "--value"])?;
    if !output.status.success() {
        return Ok(None);
    }
    let value = String::from_utf8_lossy(&output.stdout);
    let path = value.trim();
    Ok((!path.is_empty()).then(|| PathBuf::from(path)))
}

#[cfg(target_os = "linux")]
fn systemd_user_unit_loaded(path: &Path) -> Result<bool> {
    Ok(systemd_user_fragment()?.as_deref() == Some(path))
}

#[cfg(target_os = "linux")]
fn systemd_user_service_active() -> Result<bool> {
    systemctl_succeeds(["is-active", "--quiet", "rat.service"])
}

#[cfg(target_os = "linux")]
fn install_systemd_user_service(root: &Path, executable: &Path) -> Result<()> {
    require_systemd_user()?;
    let path = systemd_user_unit_path()?;
    fs::create_dir_all(path.parent().context("systemd unit path has no parent")?)
        .context("cannot create the systemd user unit directory")?;
    if let Some(fragment) = systemd_user_fragment()? {
        ensure!(
            fragment == path,
            "systemd --user already has a rat.service from {}; refusing to replace an unrelated service",
            fragment.display()
        );
    }
    if path.exists() && !systemd_unit_matches(root)? {
        bail!(
            "a different rat user service is already configured at {}; select its data root before uninstalling it",
            path.display()
        );
    }
    let was_active = systemd_user_service_active()?;
    write_private_file(&path, &systemd_unit_contents(root, executable)?)?;
    let reload = systemctl_output(&["daemon-reload"])?;
    ensure!(
        reload.status.success(),
        "systemctl --user daemon-reload failed"
    );
    let enable = systemctl_output(&["enable", "--now", "rat.service"])?;
    ensure!(
        enable.status.success(),
        "systemctl --user enable --now rat.service failed; run `rat daemon` manually if systemd --user is unavailable"
    );
    if was_active {
        let restart = systemctl_output(&["restart", "rat.service"])?;
        ensure!(
            restart.status.success(),
            "systemctl --user restart rat.service failed"
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn uninstall_systemd_user_service(root: &Path) -> Result<()> {
    require_systemd_user()?;
    let path = systemd_user_unit_path()?;
    ensure!(
        systemd_unit_matches(root)?,
        "no rat user service is installed for data root {}; refusing to alter another service",
        root.display()
    );
    if let Some(fragment) = systemd_user_fragment()? {
        ensure!(
            fragment == path,
            "systemd --user rat.service is loaded from {}; refusing to stop an unrelated service",
            fragment.display()
        );
    }
    let disable = systemctl_output(&["disable", "--now", "rat.service"])?;
    ensure!(
        disable.status.success(),
        "systemctl --user disable --now rat.service failed; the unit file was retained"
    );
    fs::remove_file(&path).with_context(|| format!("cannot remove {}", path.display()))?;
    let reload = systemctl_output(&["daemon-reload"])?;
    ensure!(
        reload.status.success(),
        "systemctl --user daemon-reload failed"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn systemd_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            '%' => quoted.push_str("%%"),
            control if control.is_control() => {
                use std::fmt::Write as _;
                let _ = write!(quoted, "\\x{:02x}", u32::from(control));
            }
            _ => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    #[test]
    fn systemd_quoting_preserves_unit_special_characters() {
        assert_eq!(
            super::systemd_quote("/tmp/rat space\\dir%\"\n"),
            "\"/tmp/rat space\\\\dir%%\\\"\\n\""
        );
        assert_eq!(
            super::systemd_quote("RAT_HOME=/tmp/a b"),
            "\"RAT_HOME=/tmp/a b\""
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn systemd_user_unit_path_uses_xdg_config_home() -> anyhow::Result<()> {
        use std::path::PathBuf;

        let path =
            super::systemd_user_unit_path_from(Some(PathBuf::from("/tmp/rat-config")), || {
                anyhow::bail!("must not need HOME when XDG_CONFIG_HOME is set")
            })?;
        assert_eq!(
            path,
            PathBuf::from("/tmp/rat-config/systemd/user/rat.service")
        );
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn launchd_xml_escaping_protects_path_values() -> anyhow::Result<()> {
        assert_eq!(
            super::xml_text("a&b<\"c'\r")?,
            "a&amp;b&lt;&quot;c&apos;&#xD;"
        );
        Ok(())
    }
}
