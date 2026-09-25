//! A lifetime marker for the installer. Multiple app windows remain supported.
use crate::i18n::tr;
use std::{mem::size_of, ptr::null_mut};
use windows_sys::Win32::{
    Foundation::{CloseHandle, LocalFree, HANDLE},
    Security::{
        Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1},
        SECURITY_ATTRIBUTES,
    },
    System::Threading::{CreateMutexExW, SYNCHRONIZATION_SYNCHRONIZE},
};

pub struct Running(HANDLE);
impl Drop for Running {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

pub fn track() -> Result<Running, String> {
    track_named("Global\\FeatherTaskManager.Running")
}

fn track_named(marker: &str) -> Result<Running, String> {
    // Authenticated users can keep the shared marker alive across sessions;
    // only administrators/SYSTEM can change its security descriptor.
    let sddl: Vec<u16> = "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x00100001;;;AU)"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut descriptor = null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            null_mut(),
        )
    } == 0
    {
        return Err(error());
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let name: Vec<u16> = marker.encode_utf16().chain(Some(0)).collect();
    let handle =
        unsafe { CreateMutexExW(&attributes, name.as_ptr(), 0, SYNCHRONIZATION_SYNCHRONIZE) };
    let failure = if handle.is_null() {
        Some(error())
    } else {
        None
    };
    unsafe { LocalFree(descriptor) };
    match failure {
        Some(error) => Err(error),
        None => Ok(Running(handle)),
    }
}

fn error() -> String {
    format!(
        "{}: {}",
        tr(
            "앱 실행 상태를 등록하지 못했습니다",
            "Could not register the running application"
        ),
        std::io::Error::last_os_error()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Threading::OpenMutexW;
    #[test]
    fn marker_survives_until_every_window_reference_is_closed() {
        let marker = format!(
            "Global\\FeatherTaskManager.Test.{}.{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let name: Vec<u16> = marker.encode_utf16().chain(Some(0)).collect();
        let first = track_named(&marker).unwrap();
        let second = track_named(&marker).unwrap();
        drop(first);
        let visible = unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, 0, name.as_ptr()) };
        assert!(!visible.is_null());
        unsafe { CloseHandle(visible) };
        drop(second);
        assert!(unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, 0, name.as_ptr()) }.is_null());
    }
}
