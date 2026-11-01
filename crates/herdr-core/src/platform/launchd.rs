//! macOS: cores and holders leave the app that started them. An app's
//! children stay in its coalition through `setsid` and double forks, and when
//! the app quits, loginwindow sends SIGTERM to all of them: quitting Herdr or a
//! terminal app ended the core and every program in it (herdr-app#219, #294).
//! A one-shot job in the user's launchd domain starts outside the app; it
//! forks the real process, which removes the job and lives on as before.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};

/// Set in the job only: its value is the job's label.
const LEAVE: &str = "HC_LAUNCHD_JOB";
const LAUNCHCTL: &str = "/bin/launchctl";

/// Starts `cmd` as a one-shot launchd job with stderr appended to `log`.
/// None: start it the plain way (an isolated session, an ssh login, no
/// desktop session, or a value a job can't carry).
pub fn spawn(cmd: &Command, log: &Path) -> Option<io::Result<()>> {
    // Isolated sessions and tests set their own runtime dir and stay on their
    // own; an ssh login runs outside any app already.
    if std::env::var_os("XDG_RUNTIME_DIR").is_some_and(|v| !v.is_empty())
        || super::SSH_LOGIN.iter().any(|v| std::env::var_os(v).is_some())
    {
        return None;
    }
    let what = cmd.get_args().next().and_then(OsStr::to_str).unwrap_or("run");
    let label = format!("dev.herdr.core.{what}.{}", &super::random_hex()[..8]);
    let here = std::env::current_dir().ok().filter(|d| d.is_dir()).unwrap_or_else(|| "/".into());
    // Read without a change: umask(2) can only swap it.
    let umask = unsafe {
        let mask = libc::umask(0o077);
        libc::umask(mask);
        mask
    };
    let plist = job(&label, cmd, &caller_env(cmd), &here, log, u32::from(umask))?;
    let file = log.with_file_name(format!("{label}.plist"));
    let loaded = std::fs::write(&file, plist).and_then(|()| {
        let domain = format!("gui/{}", unsafe { libc::getuid() });
        let out = Command::new(LAUNCHCTL).arg("bootstrap").arg(domain).arg(&file).stdin(Stdio::null()).output()?;
        if out.status.success() {
            Ok(())
        } else {
            Err(io::Error::other(String::from_utf8_lossy(&out.stderr).trim().to_owned()))
        }
    });
    let _ = std::fs::remove_file(&file);
    match loaded {
        Ok(()) => Some(Ok(())),
        Err(e) => {
            eprintln!("hc: launchctl bootstrap: {e}; the core ends with whatever started it");
            None
        }
    }
}

/// First thing in every process: one that a job started forks out of it into
/// its own session, and removes the job, so nothing of it is left loaded.
pub fn leave_job() {
    let Some(label) = std::env::var_os(LEAVE) else { return };
    // SAFETY: before main starts any thread; the parent exits without
    // running anything else.
    unsafe {
        std::env::remove_var(LEAVE);
        // launchd names the job here; programs should see what a shell sees.
        std::env::set_var("XPC_SERVICE_NAME", "0");
        match libc::fork() {
            0 => {
                libc::setsid();
            }
            // Keeps running in the job: works, but ends with logout's jobs.
            -1 => return,
            _ => libc::_exit(0),
        }
    }
    let _ = Command::new(LAUNCHCTL)
        .arg("remove")
        .arg(label)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// This process's environment with `cmd`'s changes, as the job gets it.
fn caller_env(cmd: &Command) -> BTreeMap<OsString, OsString> {
    let mut env: BTreeMap<OsString, OsString> = std::env::vars_os().collect();
    for (k, v) in cmd.get_envs() {
        match v {
            Some(v) => env.insert(k.into(), v.into()),
            None => env.remove(k),
        };
    }
    env
}

/// The job's plist. None when a value can't go into one unchanged.
fn job(
    label: &str,
    cmd: &Command,
    env: &BTreeMap<OsString, OsString>,
    here: &Path,
    log: &Path,
    umask: u32,
) -> Option<String> {
    let mut p = String::from(concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
        "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
        "<plist version=\"1.0\"><dict>\n",
    ));
    let _ = writeln!(p, "<key>Label</key><string>{}</string>", xml(label.as_ref())?);
    p.push_str("<key>ProgramArguments</key><array>\n");
    for arg in std::iter::once(cmd.get_program()).chain(cmd.get_args()) {
        let _ = writeln!(p, "<string>{}</string>", xml(arg)?);
    }
    p.push_str("</array>\n<key>EnvironmentVariables</key><dict>\n");
    for (k, v) in env.iter().filter(|(k, _)| *k != LEAVE) {
        let _ = writeln!(p, "<key>{}</key><string>{}</string>", xml(k)?, xml(v)?);
    }
    let _ = writeln!(p, "<key>{LEAVE}</key><string>{}</string>\n</dict>", xml(label.as_ref())?);
    let _ = writeln!(p, "<key>WorkingDirectory</key><string>{}</string>", xml(here.as_os_str())?);
    let _ = writeln!(p, "<key>StandardInPath</key><string>/dev/null</string>");
    let _ = writeln!(p, "<key>StandardOutPath</key><string>/dev/null</string>");
    let _ = writeln!(p, "<key>StandardErrorPath</key><string>{}</string>", xml(log.as_os_str())?);
    let _ = writeln!(p, "<key>Umask</key><integer>{umask}</integer>");
    // Runs once. The forked process outlives the job; it keeps the priority
    // of something a person uses, not a background job's.
    p.push_str("<key>RunAtLoad</key><true/>\n<key>KeepAlive</key><false/>\n");
    p.push_str("<key>AbandonProcessGroup</key><true/>\n<key>ProcessType</key><string>Interactive</string>\n");
    p.push_str("</dict></plist>\n");
    Some(p)
}

/// `s` as XML text. None for what a plist can't carry byte for byte.
fn xml(s: &OsStr) -> Option<String> {
    let s = s.to_str()?;
    if s.chars().any(|c| (c.is_control() && c != '\t' && c != '\n') || c == '\u{fffe}' || c == '\u{ffff}') {
        return None;
    }
    Some(s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_job_runs_the_command_once_with_its_environment_or_not_at_all() {
        let mut cmd = Command::new("/Users/a&b/.local/bin/herdr-core");
        cmd.arg("serve");
        let env = BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into()), (LEAVE.into(), "stale".into())]);
        let p = job("dev.herdr.core.serve.1", &cmd, &env, Path::new("/tmp"), Path::new("/s/core.log"), 0o22).unwrap();
        assert!(p.contains("<string>/Users/a&amp;b/.local/bin/herdr-core</string>\n<string>serve</string>"));
        assert!(p.contains("<key>PATH</key><string>/usr/bin:/bin</string>"));
        assert_eq!(p.matches(&format!("<key>{LEAVE}</key>")).count(), 1);
        assert!(p.contains(&format!("<key>{LEAVE}</key><string>dev.herdr.core.serve.1</string>")));
        assert!(p.contains("<key>StandardErrorPath</key><string>/s/core.log</string>"));
        assert!(p.contains("<key>Umask</key><integer>18</integer>"));
        assert!(p.contains("<key>KeepAlive</key><false/>"));
        let odd = BTreeMap::from([("X".into(), "a\u{1b}b".into())]);
        assert_eq!(job("l", &cmd, &odd, Path::new("/tmp"), Path::new("/s/core.log"), 0o22), None);
    }
}
