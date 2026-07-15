// Scheduled Task registration for the watchdog.
//
// Path : \Microsoft\Windows\SystemHealth\HealthCheck
// Trigger : logon + repetition every 1 minute forever
// Action : <exe> --watchdog
// Principal : SYSTEM, HighestAvailable, hidden
//
// We drive Task Scheduler via COM through the `windows` crate. The
// XML-based `RegisterTask` shortcut avoids marshalling per-interface
// setters (ITrigger, IAction, IPrincipal, ISettings) — the XML doc
// carries every field, and RegisterTask parses it in one call.

use std::path::Path;

use anyhow::{Context, Result};
use windows::core::BSTR;
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_MULTITHREADED,
};
use windows::Win32::System::TaskScheduler::{
    IRegisteredTask, ITaskFolder, ITaskService, TaskScheduler, TASK_CREATE_OR_UPDATE,
    TASK_LOGON_SERVICE_ACCOUNT,
};

const FOLDER_PATH: &str = r"\Microsoft\Windows\SystemHealth";
const TASK_NAME: &str = "HealthCheck";
pub const FULL_TASK_PATH: &str = r"\Microsoft\Windows\SystemHealth\HealthCheck";

pub fn register_watchdog(exe_path: &Path) -> Result<()> {
    let xml = task_xml(exe_path);
    with_com(|| {
        let service = connect_service()?;
        let folder = ensure_folder(&service, FOLDER_PATH)?;
        let _registered: IRegisteredTask = unsafe {
            folder.RegisterTask(
                &BSTR::from(TASK_NAME),
                &BSTR::from(xml.as_str()),
                TASK_CREATE_OR_UPDATE.0,
                &VARIANT::default(),
                &VARIANT::default(),
                TASK_LOGON_SERVICE_ACCOUNT,
                &VARIANT::default(),
            )
        }
        .context("ITaskFolder::RegisterTask")?;
        tracing::info!("watchdog task registered at {FULL_TASK_PATH}");
        Ok(())
    })
}

pub fn delete_watchdog() -> Result<()> {
    with_com(|| {
        let service = connect_service()?;
        let folder = match get_folder(&service, FOLDER_PATH) {
            Ok(f) => f,
            Err(_) => {
                tracing::debug!("watchdog folder not found - nothing to delete");
                return Ok(());
            }
        };
        match unsafe { folder.DeleteTask(&BSTR::from(TASK_NAME), 0) } {
            Ok(()) => tracing::info!("watchdog task deleted"),
            Err(e) => tracing::warn!(error = %e, "delete watchdog task"),
        }
        Ok(())
    })
}

pub fn update_watchdog_action(exe_path: &Path) -> Result<()> {
    // Delete + recreate — the ITaskDefinition mutation path is verbose
    // and RegisterTask with TASK_CREATE_OR_UPDATE is idempotent.
    delete_watchdog().ok();
    register_watchdog(exe_path)
}

fn with_com<F, R>(body: F) -> Result<R>
where
    F: FnOnce() -> Result<R>,
{
    use windows::core::HRESULT;
    // RPC_E_CHANGED_MODE : COM is already initialized on this thread in a
    // different apartment mode (e.g. eframe's wgpu backend initialized STA
    // before we reached here during the uninstall flow). COM is usable —
    // we just don't own the init and must skip the paired CoUninitialize.
    const RPC_E_CHANGED_MODE: HRESULT = HRESULT(0x80010106u32 as i32);
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let we_own_init = if hr == RPC_E_CHANGED_MODE {
        false
    } else {
        hr.ok().context("CoInitializeEx")?;
        true
    };
    let result = body();
    if we_own_init {
        unsafe { CoUninitialize() };
    }
    result
}

fn connect_service() -> Result<ITaskService> {
    let service: ITaskService =
        unsafe { CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER) }
            .context("CoCreateInstance(TaskScheduler)")?;
    unsafe {
        service.Connect(
            &VARIANT::default(),
            &VARIANT::default(),
            &VARIANT::default(),
            &VARIANT::default(),
        )
    }
    .context("ITaskService::Connect")?;
    Ok(service)
}

fn get_folder(service: &ITaskService, path: &str) -> Result<ITaskFolder> {
    unsafe { service.GetFolder(&BSTR::from(path)) }
        .with_context(|| format!("ITaskService::GetFolder({path})"))
}

fn ensure_folder(service: &ITaskService, path: &str) -> Result<ITaskFolder> {
    if let Ok(f) = get_folder(service, path) {
        return Ok(f);
    }
    // Walk parents to create missing intermediate folders. Task Scheduler
    // requires the parent folder to exist before CreateFolder can add a
    // child.
    let mut parts = path.trim_start_matches('\\').split('\\');
    let mut current = String::from("\\");
    let root = get_folder(service, &current)?;
    let mut cursor = root;
    while let Some(seg) = parts.next() {
        if seg.is_empty() {
            continue;
        }
        if current == "\\" {
            current.push_str(seg);
        } else {
            current.push('\\');
            current.push_str(seg);
        }
        cursor = match get_folder(service, &current) {
            Ok(f) => f,
            Err(_) => unsafe { cursor.CreateFolder(&BSTR::from(seg), &VARIANT::default()) }
                .with_context(|| format!("CreateFolder({seg}) under {current}"))?,
        };
    }
    Ok(cursor)
}

fn task_xml(exe_path: &Path) -> String {
    // Escape XML meta-chars in the path. Paths may contain `&` in rare
    // cases (username with `&`) — belt-and-braces.
    let exe = xml_escape(&exe_path.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Monitors user session health metrics for ergonomic notifications.</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <Repetition>
        <Interval>PT1M</Interval>
        <StopAtDurationEnd>false</StopAtDurationEnd>
      </Repetition>
    </LogonTrigger>
    <TimeTrigger>
      <Enabled>true</Enabled>
      <StartBoundary>2020-01-01T00:00:00</StartBoundary>
      <Repetition>
        <Interval>PT1M</Interval>
        <StopAtDurationEnd>false</StopAtDurationEnd>
      </Repetition>
    </TimeTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>S-1-5-18</UserId>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>true</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe}</Command>
      <Arguments>--watchdog</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn xml_escapes_special_chars() {
        let s = xml_escape("A&B<C>\"D'");
        assert_eq!(s, "A&amp;B&lt;C&gt;&quot;D&apos;");
    }

    #[test]
    fn xml_contains_watchdog_argument() {
        let xml = task_xml(&PathBuf::from(r"C:\Users\alice\Desktop\SystemHealthAgent.exe"));
        assert!(xml.contains(r"<Arguments>--watchdog</Arguments>"));
        assert!(xml.contains(r"<Command>C:\Users\alice\Desktop\SystemHealthAgent.exe</Command>"));
        assert!(xml.contains("<Hidden>true</Hidden>"));
        assert!(xml.contains("S-1-5-18")); // SYSTEM SID
        assert!(xml.contains("<Interval>PT1M</Interval>"));
    }

    #[test]
    fn task_xml_contains_both_triggers() {
        // Bug #4 regression : LogonTrigger alone means the task never
        // fires when the user is already logged in at install time,
        // because LogonTrigger's initial fire event has already passed.
        // TimeTrigger with a past StartBoundary + PT1M repetition
        // catches the mid-session install case and makes the watchdog
        // tick immediately.
        let xml = task_xml(&PathBuf::from(r"C:\x\SystemHealthAgent.exe"));
        assert!(xml.contains("<LogonTrigger>"));
        assert!(xml.contains("<TimeTrigger>"));
        assert!(xml.contains("<StartBoundary>2020-01-01T00:00:00</StartBoundary>"));
        // Both triggers must have the PT1M repetition.
        assert_eq!(xml.matches("<Interval>PT1M</Interval>").count(), 2);
    }
}
