use std::time::{Duration, Instant};

use anyhow::Context;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProcessIdentity {
    pub pid: i32,
    pub pgid: i32,
    pub linux_boot_id: String,
    pub proc_start_time_ticks: u64,
}

#[cfg(target_os = "linux")]
pub fn read_process_identity(pid: i32) -> anyhow::Result<ProcessIdentity> {
    let linux_boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
        .trim()
        .to_string();
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let close = stat
        .rfind(')')
        .context("process stat has no command terminator")?;
    let fields = stat[close + 1..].split_whitespace().collect::<Vec<_>>();
    let proc_start_time_ticks = fields
        .get(19)
        .context("process stat has no start-time field")?
        .parse()?;
    let pgid = fields
        .get(2)
        .context("process stat has no process-group field")?
        .parse()?;
    Ok(ProcessIdentity {
        pid,
        pgid,
        linux_boot_id,
        proc_start_time_ticks,
    })
}

#[cfg(not(target_os = "linux"))]
pub fn read_process_identity(pid: i32) -> anyhow::Result<ProcessIdentity> {
    Ok(ProcessIdentity {
        pid,
        pgid: pid,
        linux_boot_id: "not-applicable".to_string(),
        proc_start_time_ticks: 0,
    })
}

#[cfg(target_os = "linux")]
fn process_is_zombie(pid: i32) -> anyhow::Result<bool> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let close = stat
        .rfind(')')
        .context("process stat has no command terminator")?;
    Ok(stat[close + 1..]
        .split_whitespace()
        .next()
        .is_some_and(|state| state == "Z"))
}

#[cfg(target_os = "linux")]
struct OwnedPidFd {
    fd: i32,
    identity: ProcessIdentity,
}

#[cfg(target_os = "linux")]
impl Drop for OwnedPidFd {
    fn drop(&mut self) {
        unsafe extern "C" {
            fn close(fd: i32) -> i32;
        }
        let _ = unsafe { close(self.fd) };
    }
}

#[cfg(target_os = "linux")]
fn open_owned_pidfd(identity: ProcessIdentity) -> anyhow::Result<Option<OwnedPidFd>> {
    const SYS_PIDFD_OPEN: i64 = 434;
    unsafe extern "C" {
        fn syscall(number: i64, ...) -> i64;
    }
    let fd = unsafe { syscall(SYS_PIDFD_OPEN, identity.pid, 0_u32) };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(error).context("pidfd_open owned process");
    }
    let owned = OwnedPidFd {
        fd: i32::try_from(fd)?,
        identity,
    };
    match read_process_identity(owned.identity.pid) {
        Ok(current)
            if current.linux_boot_id == owned.identity.linux_boot_id
                && current.proc_start_time_ticks == owned.identity.proc_start_time_ticks
                && current.pgid == owned.identity.pgid =>
        {
            Ok(Some(owned))
        }
        Ok(_) => Ok(None),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "linux")]
fn owned_process_group_members(pgid: i32, linux_boot_id: &str) -> anyhow::Result<Vec<OwnedPidFd>> {
    let mut members = Vec::new();
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|value| value.parse::<i32>().ok())
        else {
            continue;
        };
        let identity = match read_process_identity(pid) {
            Ok(identity) if identity.pgid == pgid && identity.linux_boot_id == linux_boot_id => {
                identity
            }
            _ => continue,
        };
        if process_is_zombie(pid).unwrap_or(false) {
            continue;
        }
        if let Some(member) = open_owned_pidfd(identity)? {
            members.push(member);
        }
    }
    Ok(members)
}

#[cfg(target_os = "linux")]
fn signal_pidfds(members: &[OwnedPidFd], signal: i32) -> anyhow::Result<()> {
    const SYS_PIDFD_SEND_SIGNAL: i64 = 424;
    unsafe extern "C" {
        fn syscall(number: i64, ...) -> i64;
    }
    for member in members {
        let result = unsafe {
            syscall(
                SYS_PIDFD_SEND_SIGNAL,
                member.fd,
                signal,
                std::ptr::null::<std::ffi::c_void>(),
                0_u32,
            )
        };
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(error)
                    .with_context(|| format!("pidfd_send_signal process {}", member.identity.pid));
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub async fn verify_process_group_absent(identity: &ProcessIdentity) -> anyhow::Result<()> {
    const SIGTERM: i32 = 15;
    const SIGKILL: i32 = 9;
    match read_process_identity(identity.pid) {
        Ok(current) => anyhow::ensure!(
            current.linux_boot_id == identity.linux_boot_id
                && current.proc_start_time_ticks == identity.proc_start_time_ticks
                && current.pgid == identity.pgid,
            "persisted process leader identity changed; refusing PID-reuse signal"
        ),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) => {}
        Err(error) => return Err(error),
    }
    let members = owned_process_group_members(identity.pgid, &identity.linux_boot_id)?;
    signal_pidfds(&members, SIGTERM)?;
    let grace = Instant::now() + Duration::from_secs(2);
    while Instant::now() < grace {
        if owned_process_group_members(identity.pgid, &identity.linux_boot_id)?.is_empty() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    for _ in 0..8 {
        let members = owned_process_group_members(identity.pgid, &identity.linux_boot_id)?;
        if members.is_empty() {
            return Ok(());
        }
        signal_pidfds(&members, SIGKILL)?;
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    anyhow::ensure!(
        owned_process_group_members(identity.pgid, &identity.linux_boot_id)?.is_empty(),
        "owned process group remains after pidfd SIGKILL"
    );
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub async fn verify_process_group_absent(_identity: &ProcessIdentity) -> anyhow::Result<()> {
    anyhow::bail!("persisted process recovery requires Linux process identity")
}
