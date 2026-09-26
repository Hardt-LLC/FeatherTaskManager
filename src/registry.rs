//! Registry handles that never traverse symbolic links, including during creation.
//!
//! HKCU is writable by unelevated software even when Feather runs elevated. Each
//! component is opened relative to a pinned parent with OBJ_DONT_REPARSE. The
//! same kernel check applies to NtCreateKey, so inserting a link between an
//! unsuccessful open and creation cannot redirect a write or create a subkey
//! elsewhere. An unfinished link also fails; inspecting SymbolicLinkValue alone
//! would leave a race before that value is populated.
//! This helper accepts native registry paths only; callers must explicitly map
//! any supported redirected locations. Native NtOpenKey does not implement the
//! Win32 WOW64 view selection, so KEY_WOW64_32KEY is rejected rather than ignored.
//!
//! https://learn.microsoft.com/windows/win32/api/ntdef/ns-ntdef-_object_attributes
//! https://learn.microsoft.com/windows-hardware/drivers/ddi/wdm/nf-wdm-zwcreatekey

use std::{ffi::c_void, mem::size_of, ptr::null_mut};
use windows_sys::Win32::{Foundation::*, System::Registry::*};

#[repr(C)]
struct ObjectAttributes {
    length: u32,
    root: HKEY,
    name: *const UNICODE_STRING,
    attributes: u32,
    security_descriptor: *const c_void,
    security_quality_of_service: *const c_void,
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtOpenKey(key: *mut HKEY, access: u32, attributes: *const ObjectAttributes) -> NTSTATUS;
    fn NtCreateKey(
        key: *mut HKEY,
        access: u32,
        attributes: *const ObjectAttributes,
        title_index: u32,
        class: *const UNICODE_STRING,
        options: u32,
        disposition: *mut u32,
    ) -> NTSTATUS;
}

#[derive(Debug)]
pub struct Key(pub HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}

fn native(parent: HKEY, name: &str, access: u32, create: bool) -> Result<Key, u32> {
    let mut text: Vec<u16> = name.encode_utf16().collect();
    let length = text
        .len()
        .checked_mul(2)
        .filter(|len| *len <= u16::MAX as usize)
        .ok_or(ERROR_INVALID_PARAMETER)? as u16;
    if text.is_empty() || text.contains(&0) {
        return Err(ERROR_INVALID_PARAMETER);
    }
    let name = UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        Buffer: text.as_mut_ptr(),
    };
    let attributes = ObjectAttributes {
        length: size_of::<ObjectAttributes>() as u32,
        root: parent,
        name: &name,
        attributes: OBJ_CASE_INSENSITIVE | OBJ_DONT_REPARSE,
        security_descriptor: std::ptr::null(),
        security_quality_of_service: std::ptr::null(),
    };
    let mut key = null_mut();
    let status = unsafe {
        if create {
            NtCreateKey(
                &mut key,
                access,
                &attributes,
                0,
                std::ptr::null(),
                REG_OPTION_NON_VOLATILE,
                null_mut(),
            )
        } else {
            NtOpenKey(&mut key, access, &attributes)
        }
    };
    if status < 0 {
        Err(unsafe { RtlNtStatusToDosError(status) })
    } else if key.is_null() {
        Err(ERROR_INVALID_HANDLE)
    } else {
        Ok(Key(key))
    }
}

fn root(hive: HKEY, view: u32) -> Result<Key, u32> {
    if hive == HKEY_CURRENT_USER {
        let mut key = null_mut();
        let status = unsafe { RegOpenCurrentUser(KEY_QUERY_VALUE | view, &mut key) };
        if status == ERROR_SUCCESS {
            Ok(Key(key))
        } else {
            Err(status)
        }
    } else if hive == HKEY_LOCAL_MACHINE {
        native(
            null_mut(),
            r"\Registry\Machine",
            KEY_QUERY_VALUE | view,
            false,
        )
    } else {
        Err(ERROR_INVALID_PARAMETER)
    }
}

fn traverse(hive: HKEY, path: &str, access: u32, create: bool) -> Result<Key, u32> {
    if access & KEY_WOW64_32KEY != 0 {
        return Err(ERROR_INVALID_PARAMETER);
    }
    let components: Vec<_> = path.split('\\').collect();
    if components
        .iter()
        .any(|part| part.is_empty() || part.contains('\0'))
    {
        return Err(ERROR_INVALID_PARAMETER);
    }
    let view = access & (KEY_WOW64_32KEY | KEY_WOW64_64KEY);
    let mut parent = root(hive, view)?;
    for (index, component) in components.iter().enumerate() {
        let wanted = if index + 1 == components.len() {
            access
        } else {
            KEY_QUERY_VALUE | view
        };
        parent = match native(parent.0, component, wanted, false) {
            Ok(key) => key,
            Err(ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) if create => {
                native(parent.0, component, wanted, true)?
            }
            Err(code) => return Err(code),
        };
    }
    Ok(parent)
}

pub fn open(hive: HKEY, path: &str, access: u32) -> Result<Option<Key>, u32> {
    match traverse(hive, path, access, false) {
        Ok(key) => Ok(Some(key)),
        Err(ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) => Ok(None),
        Err(code) => Err(code),
    }
}

pub fn create(hive: HKEY, path: &str, access: u32) -> Result<Key, u32> {
    traverse(hive, path, access, true)
}

#[cfg(test)]
pub mod test_support {
    use super::*;
    use std::{
        ptr::null,
        sync::atomic::{AtomicU64, Ordering},
    };
    static NEXT: AtomicU64 = AtomicU64::new(0);

    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtDeleteKey(key: HKEY) -> NTSTATUS;
        fn NtQueryKey(
            key: HKEY,
            class: u32,
            buffer: *mut c_void,
            bytes: u32,
            needed: *mut u32,
        ) -> NTSTATUS;
    }
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    pub fn key_name(key: &Key) -> Vec<u8> {
        let mut buffer = vec![0u32; 8192];
        let mut needed = 0;
        assert_eq!(
            unsafe {
                NtQueryKey(
                    key.0,
                    3,
                    buffer.as_mut_ptr().cast(),
                    (buffer.len() * 4) as u32,
                    &mut needed,
                )
            },
            0
        );
        let bytes = buffer[0];
        assert!(needed >= 4 && bytes <= needed - 4 && bytes <= (buffer.len() * 4 - 4) as u32);
        buffer[1..]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .take(bytes as usize)
            .collect()
    }

    pub struct Fixture {
        pub path: String,
    }
    impl Fixture {
        pub fn new() -> Self {
            let path = format!(
                r"Software\FeatherTask\Tests\RegistryLinks-{}-{}-{}",
                std::process::id(),
                unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() },
                NEXT.fetch_add(1, Ordering::Relaxed)
            );
            drop(create(HKEY_CURRENT_USER, &path, KEY_ALL_ACCESS).unwrap());
            Self { path }
        }
        pub fn path(&self, child: &str) -> String {
            format!(r"{}\{child}", self.path)
        }
        pub fn key(&self, child: &str) -> Key {
            create(HKEY_CURRENT_USER, &self.path(child), KEY_ALL_ACCESS).unwrap()
        }
        pub fn link(&self, child: &str, target: Option<&Key>) -> Link {
            let mut key = null_mut();
            assert_eq!(
                unsafe {
                    RegCreateKeyExW(
                        HKEY_CURRENT_USER,
                        wide(&self.path(child)).as_ptr(),
                        0,
                        null(),
                        REG_OPTION_CREATE_LINK,
                        KEY_ALL_ACCESS,
                        null(),
                        &mut key,
                        null_mut(),
                    )
                },
                ERROR_SUCCESS
            );
            let link = Link(Key(key));
            if let Some(target) = target {
                let target_name = key_name(target);
                assert_eq!(
                    unsafe {
                        RegSetValueExW(
                            link.0 .0,
                            wide("SymbolicLinkValue").as_ptr(),
                            0,
                            REG_LINK,
                            target_name.as_ptr(),
                            target_name.len() as u32,
                        )
                    },
                    ERROR_SUCCESS
                );
            }
            link
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide(&self.path).as_ptr()) };
        }
    }
    pub struct Link(Key);
    impl Drop for Link {
        fn drop(&mut self) {
            // Delete the link object by its handle, never recursively follow it.
            assert_eq!(unsafe { NtDeleteKey(self.0 .0) }, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{test_support::Fixture, *};

    #[test]
    fn component_traversal_preserves_native_registry_paths() {
        for hive in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
            for view in [KEY_WOW64_64KEY] {
                for path in [
                    r"Software\Microsoft",
                    r"Software\Microsoft\Windows\CurrentVersion\Run",
                    r"Software\Wow6432Node\Microsoft\Windows\CurrentVersion\Run",
                ] {
                    let name: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
                    let mut handle = null_mut();
                    let status = unsafe {
                        RegOpenKeyExW(hive, name.as_ptr(), 0, KEY_QUERY_VALUE | view, &mut handle)
                    };
                    let safe = open(hive, path, KEY_QUERY_VALUE | view).unwrap();
                    if status == ERROR_FILE_NOT_FOUND {
                        assert!(safe.is_none());
                    } else {
                        assert_eq!(status, ERROR_SUCCESS);
                        let ordinary = Key(handle);
                        let text = |key: &Key| {
                            String::from_utf16(
                                &test_support::key_name(key)
                                    .as_chunks::<2>()
                                    .0
                                    .iter()
                                    .map(|b| u16::from_le_bytes([b[0], b[1]]))
                                    .collect::<Vec<_>>(),
                            )
                            .unwrap()
                            .to_lowercase()
                        };
                        assert_eq!(text(&safe.unwrap()), text(&ordinary));
                    }
                }
            }
        }
        assert_eq!(
            open(
                HKEY_LOCAL_MACHINE,
                "Software",
                KEY_QUERY_VALUE | KEY_WOW64_32KEY
            )
            .unwrap_err(),
            ERROR_INVALID_PARAMETER
        );
    }

    #[test]
    fn creation_and_open_reject_leaf_ancestor_and_unfinished_links() {
        let fixture = Fixture::new();
        let target = fixture.key("UnrelatedTarget");
        let _child = fixture.key(r"UnrelatedTarget\Existing");
        let _link = fixture.link("Link", Some(&target));
        let _unfinished = fixture.link("UnfinishedLink", None);
        for path in [
            "Link",
            r"Link\Existing",
            r"Link\Missing",
            "UnfinishedLink",
            r"UnfinishedLink\Missing",
        ] {
            assert!(!matches!(
                open(HKEY_CURRENT_USER, &fixture.path(path), KEY_SET_VALUE),
                Ok(Some(_))
            ));
            assert!(create(HKEY_CURRENT_USER, &fixture.path(path), KEY_SET_VALUE).is_err());
        }
        assert!(open(
            HKEY_CURRENT_USER,
            &fixture.path(r"UnrelatedTarget\Missing"),
            KEY_QUERY_VALUE
        )
        .unwrap()
        .is_none());
        // Legitimate absent ancestors are created and can be reopened.
        drop(fixture.key(r"Regular\Nested\Leaf"));
        assert!(open(
            HKEY_CURRENT_USER,
            &fixture.path(r"Regular\Nested\Leaf"),
            KEY_QUERY_VALUE
        )
        .unwrap()
        .is_some());
    }

    #[test]
    fn link_inserted_after_missing_lookup_is_rejected_by_native_create() {
        let fixture = Fixture::new();
        let parent = fixture.key("Parent");
        let target = fixture.key("UnrelatedTarget");
        assert!(native(parent.0, "Approval", KEY_SET_VALUE, false).is_err());
        let _link = fixture.link(r"Parent\Approval", Some(&target));
        assert_eq!(
            native(parent.0, "Approval", KEY_SET_VALUE, true).unwrap_err(),
            ERROR_REPARSE_POINT_ENCOUNTERED
        );
    }
}
