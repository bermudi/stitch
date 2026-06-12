use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Platform {
    pub os: String,
    pub arch: String,
    pub distro: Option<String>,
    pub hostname: String,
    pub shell: String,
}

impl Platform {
    pub fn detect() -> Self {
        let os = std::env::consts::OS.to_string();
        let arch = std::env::consts::ARCH.to_string();
        let hostname = std::process::Command::new("hostname")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "unknown".into());
        let shell = detect_shell();
        let distro = detect_distro();

        Self { os, arch, distro, hostname, shell }
    }

    pub fn matches_when(&self, when: &crate::config::WhenClause) -> bool {
        if let Some(ref os) = when.os {
            if &self.os != os { return false; }
        }
        if let Some(ref arch) = when.arch {
            if &self.arch != arch { return false; }
        }
        if let Some(ref distro) = when.distro {
            if self.distro.as_deref() != Some(distro.as_str()) { return false; }
        }
        if let Some(ref hostname) = when.hostname {
            if &self.hostname != hostname { return false; }
        }
        if let Some(ref shell) = when.shell {
            if &self.shell != shell { return false; }
        }
        true
    }
}

fn detect_shell() -> String {
    // $SHELL is the most reliable cross-platform indicator.
    std::env::var("SHELL")
        .ok()
        .and_then(|s| {
            PathBuf::from(&s)
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "unknown".into())
}

fn detect_distro() -> Option<String> {
    let os = std::env::consts::OS;
    match os {
        "linux" => {
            // Try /etc/os-release first, then /etc/lsb-release
            if let Ok(contents) = std::fs::read_to_string("/etc/os-release") {
                for line in contents.lines() {
                    if let Some(id) = line.strip_prefix("ID=") {
                        return Some(id.trim_matches('"').to_string());
                    }
                }
            }
            if let Ok(contents) = std::fs::read_to_string("/etc/lsb-release") {
                for line in contents.lines() {
                    if let Some(id) = line.strip_prefix("DISTRIB_ID=") {
                        return Some(id.trim_matches('"').to_lowercase());
                    }
                }
            }
            None
        }
        "macos" => Some("macos".into()),
        "windows" => Some("windows".into()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WhenClause;

    #[test]
    fn test_platform_detects() {
        let p = Platform::detect();
        assert!(!p.os.is_empty());
        assert!(!p.arch.is_empty());
        assert!(!p.hostname.is_empty());
    }

    #[test]
    fn test_matches_when_empty() {
        let p = Platform::detect();
        let when = WhenClause::default();
        assert!(p.matches_when(&when));
    }

    #[test]
    fn test_matches_when_os() {
        let p = Platform::detect();
        let when = WhenClause {
            os: Some(p.os.clone()),
            ..Default::default()
        };
        assert!(p.matches_when(&when));

        let when_wrong = WhenClause {
            os: Some("definitely_not_real_os".into()),
            ..Default::default()
        };
        assert!(!p.matches_when(&when_wrong));
    }
}
