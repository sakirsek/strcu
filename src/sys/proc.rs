//! Ties child processes to strcu: if strcu exits or crashes, cloudflared exits too.
//! That way no orphaned tunnel is left on the internet.

use std::os::windows::io::AsRawHandle;
use std::process::Child;
use std::sync::OnceLock;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows::core::PCWSTR;

/// Handle of the job object; never closed. When strcu exits, Windows closes it and kills what is inside.
static JOB: OnceLock<Option<isize>> = OnceLock::new();

fn job() -> Option<HANDLE> {
    let raw = JOB.get_or_init(|| unsafe {
        let job = CreateJobObjectW(None, PCWSTR::null()).ok()?;
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
        .ok()?;
        Some(job.0 as isize)
    });
    raw.map(|h| HANDLE(h as *mut _))
}

/// Ties the process to strcu's lifetime. If this fails the process still runs, it is just only stopped
/// on Drop.
pub fn tie(child: &Child) {
    if let Some(job) = job() {
        unsafe {
            let _ = AssignProcessToJobObject(job, HANDLE(child.as_raw_handle()));
        }
    }
}
