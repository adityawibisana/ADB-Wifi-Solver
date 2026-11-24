//! Windows Defender Firewall help: opens the settings pages, reports the
//! network profiles, and adds a rule that lets adb receive mDNS replies.
//! Everything here is Windows only; elsewhere the functions return an error.

use serde::Serialize;
use std::path::Path;

/// The display name of the rule [`allow`] adds. Re-running replaces it.
pub const RULE_NAME: &str = "adb (adbqr)";

/// A network Windows is connected to and the profile it applies.
#[derive(Serialize)]
pub struct Network {
    pub name: String,
    /// `Public`, `Private` or `DomainAuthenticated`.
    pub category: String,
}

/// Opens a Windows settings page: `"firewall"` (Allowed apps) or `"network"`.
pub fn open_settings(page: &str) -> Result<(), String> {
    if !cfg!(windows) {
        return Err("Only available on Windows".into());
    }
    let (program, args): (&str, &[&str]) = match page {
        "firewall" => ("control.exe", &["/name", "Microsoft.WindowsFirewall", "/page", "pageConfigureApps"]),
        "network" => ("explorer.exe", &["ms-settings:network-status"]),
        _ => return Err(format!("unknown settings page {page}")),
    };
    std::process::Command::new(program)
        .args(args)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("could not open settings: {e}"))
}

/// The networks Windows is connected to, with their profile.
pub fn networks() -> Result<Vec<Network>, String> {
    if !cfg!(windows) {
        return Err("Only available on Windows".into());
    }
    let out = powershell(
        "Get-NetConnectionProfile | ForEach-Object { \"$($_.Name)`t$($_.NetworkCategory)\" }",
    )?;
    Ok(out
        .lines()
        .filter_map(|l| l.trim().split_once('\t'))
        .map(|(name, category)| Network { name: name.to_string(), category: category.to_string() })
        .collect())
}

/// Removes the block rules Windows made for this adb (when its "allow
/// access?" prompt was cancelled) and adds an inbound allow rule for private
/// and domain networks. Runs elevated, so Windows shows a UAC prompt.
/// Returns what it did.
pub fn allow(adb: &Path) -> Result<String, String> {
    if !cfg!(windows) {
        return Err("Only available on Windows".into());
    }
    let result_file = std::env::temp_dir().join(format!("adbqr-firewall-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&result_file);

    let script = format!(
        r#"$ErrorActionPreference = 'Stop'
$out = {out}
try {{
  $adb = [IO.Path]::GetFullPath({adb})
  $blocks = @(Get-NetFirewallApplicationFilter |
    Where-Object {{ $_.Program -and ([Environment]::ExpandEnvironmentVariables($_.Program) -ieq $adb) }} |
    Get-NetFirewallRule | Where-Object {{ $_.Action -eq 'Block' }})
  $blocks | Remove-NetFirewallRule
  Get-NetFirewallRule -DisplayName {name} -ErrorAction SilentlyContinue | Remove-NetFirewallRule
  New-NetFirewallRule -DisplayName {name} -Description 'Lets adb find phones over Wi-Fi (mDNS, UDP 5353). Added by adbqr.' `
    -Direction Inbound -Program $adb -Action Allow -Profile Private,Domain | Out-Null
  Set-Content -Path $out -Encoding UTF8 -Value "ok $($blocks.Count)"
}} catch {{
  Set-Content -Path $out -Encoding UTF8 -Value "error $($_.Exception.Message)"
  exit 1
}}"#,
        out = ps_quote(&result_file.display().to_string()),
        adb = ps_quote(&adb.display().to_string()),
        name = ps_quote(RULE_NAME),
    );
    // Start-Process -Verb RunAs is what brings up UAC. It throws if the user says no.
    let launcher = format!(
        "try {{ $p = Start-Process powershell.exe -Verb RunAs -Wait -PassThru -WindowStyle Hidden \
         -ArgumentList '-NoProfile','-NonInteractive','-ExecutionPolicy','Bypass','-EncodedCommand','{}' -ErrorAction Stop }} \
         catch {{ exit 2 }}; exit $p.ExitCode",
        encode_command(&script)
    );
    let status = powershell_status(&launcher);
    let written = std::fs::read_to_string(&result_file).unwrap_or_default();
    let _ = std::fs::remove_file(&result_file);
    let written = written.trim_start_matches('\u{feff}').trim();

    if status == Some(2) {
        return Err("Cancelled: Windows asked for administrator approval and it wasn't given.".into());
    }
    if let Some(msg) = written.strip_prefix("error ") {
        return Err(format!("Could not change the firewall: {msg}"));
    }
    let Some(count) = written.strip_prefix("ok ") else {
        return Err(format!("Could not change the firewall (exit code {status:?})."));
    };
    let removed = match count.trim().parse::<u32>().unwrap_or(0) {
        0 => String::new(),
        1 => " Removed 1 rule that blocked adb.".into(),
        n => format!(" Removed {n} rules that blocked adb."),
    };
    Ok(format!("Added the firewall rule \"{RULE_NAME}\" for private networks.{removed}"))
}

/// Quotes a string for PowerShell as a literal ('...' with ' doubled).
fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// `-EncodedCommand` takes base64 of UTF-16LE, which sidesteps all quoting.
fn encode_command(script: &str) -> String {
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64(&bytes)
}

fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, &b)| n | (b as u32) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ABC[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn powershell_command(script: &str) -> std::process::Command {
    let mut command = std::process::Command::new("powershell.exe");
    crate::adb::hide_console(&mut command);
    command
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand"])
        .arg(encode_command(script))
        .stdin(std::process::Stdio::null());
    command
}

fn powershell(script: &str) -> Result<String, String> {
    let out = powershell_command(script).output().map_err(|e| format!("could not run PowerShell: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!("PowerShell failed: {}", String::from_utf8_lossy(&out.stderr).trim()))
    }
}

fn powershell_status(script: &str) -> Option<i32> {
    powershell_command(script)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()
        .and_then(|s| s.code())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_values() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn quotes_single_quotes() {
        assert_eq!(ps_quote(r"C:\it's\adb.exe"), r"'C:\it''s\adb.exe'");
    }
}
