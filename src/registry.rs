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
//! [`delete_tree`] removes a key tree the same way and deletes a link in it as
//! the link itself (OBJ_OPENLINK), never touching its target.
//!
//! https://learn.microsoft.com/windows/win32/api/ntdef/ns-ntdef-_object_attributes
//! https://learn.microsoft.com/windows-hardware/drivers/ddi/wdm/nf-wdm-zwcreatekey

use std::{
    ffi::c_void,
    mem::size_of,
    ptr::{null, null_mut},
};
use windows_sys::Win32::{Foundation::*, Storage::FileSystem::DELETE, System::Registry::*};

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
    fn NtDeleteKey(key: HKEY) -> NTSTATUS;
}

#[derive(Debug)]
pub struct Key(pub HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}

fn native(parent: HKEY, name: &str, access: u32, create: bool) -> Result<Key, u32> {
    let text: Vec<u16> = name.encode_utf16().collect();
    native_units(parent, &text, access, OBJ_DONT_REPARSE, create)
}

/// Open or create the component `text` relative to `parent`. `flags` is
/// OBJ_DONT_REPARSE, alone or with OBJ_OPENLINK (open a link as itself).
fn native_units(
    parent: HKEY,
    text: &[u16],
    access: u32,
    flags: u32,
    create: bool,
) -> Result<Key, u32> {
    let length = text
        .len()
        .checked_mul(2)
        .filter(|len| *len <= u16::MAX as usize)
        .ok_or(ERROR_INVALID_PARAMETER)? as u16;
    if text.is_empty() || text.contains(&0) || flags & OBJ_DONT_REPARSE == 0 {
        return Err(ERROR_INVALID_PARAMETER);
    }
    let name = UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        // The kernel only reads the name.
        Buffer: text.as_ptr().cast_mut(),
    };
    let attributes = ObjectAttributes {
        length: size_of::<ObjectAttributes>() as u32,
        root: parent,
        name: &name,
        attributes: OBJ_CASE_INSENSITIVE | flags,
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

/// Levels below the key that [`delete_tree`] descends to at most.
const DELETE_MAX_DEPTH: usize = 32;
/// Keys (the removed key included) that one [`delete_tree`] removes at most.
const DELETE_MAX_KEYS: usize = 10_000;

/// Delete the key `path` (at least `Parent\Name`) of `hive` with all its
/// subkeys and values. Succeeds when the key is gone, including when it or
/// an ancestor never existed.
///
/// No registry link is ever followed. The ancestors are opened like
/// [`open`], so a link among them fails the call. The key and every subkey
/// are opened one component at a time below their pinned parent with
/// OBJ_OPENLINK | OBJ_DONT_REPARSE, which yields the named key itself: a link
/// (finished or not) is enumerated and deleted as the link object, and its
/// target is never opened, enumerated or deleted. Deeper than
/// DELETE_MAX_DEPTH levels or larger than DELETE_MAX_KEYS keys fails with
/// ERROR_STACK_OVERFLOW / ERROR_NOT_ENOUGH_QUOTA; subtrees finished before
/// that stay deleted.
pub fn delete_tree(hive: HKEY, path: &str) -> Result<(), u32> {
    delete_tree_within(hive, path, DELETE_MAX_DEPTH, DELETE_MAX_KEYS)
}

fn delete_tree_within(
    hive: HKEY,
    path: &str,
    max_depth: usize,
    max_keys: usize,
) -> Result<(), u32> {
    let (ancestors, name) = path.rsplit_once('\\').ok_or(ERROR_INVALID_PARAMETER)?;
    let parent = match traverse(hive, ancestors, KEY_QUERY_VALUE, false) {
        Ok(parent) => parent,
        Err(ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) => return Ok(()),
        Err(code) => return Err(code),
    };
    let name: Vec<u16> = name.encode_utf16().collect();
    let mut budget = max_keys;
    delete_subtree(&parent, &name, max_depth, &mut budget)
}

/// Delete the subkey `name` of `parent` (a single component) and everything
/// below it, depth first; `depth` levels may still follow below it.
fn delete_subtree(parent: &Key, name: &[u16], depth: usize, budget: &mut usize) -> Result<(), u32> {
    if name.contains(&u16::from(b'\\')) {
        return Err(ERROR_INVALID_PARAMETER);
    }
    let key = match native_units(
        parent.0,
        name,
        DELETE | KEY_ENUMERATE_SUB_KEYS,
        OBJ_OPENLINK | OBJ_DONT_REPARSE,
        false,
    ) {
        Ok(key) => key,
        Err(ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) => return Ok(()),
        Err(code) => return Err(code),
    };
    *budget = budget.checked_sub(1).ok_or(ERROR_NOT_ENOUGH_QUOTA)?;
    let children = subkey_names(&key, *budget)?;
    if !children.is_empty() {
        let below = depth.checked_sub(1).ok_or(ERROR_STACK_OVERFLOW)?;
        for child in &children {
            delete_subtree(&key, child, below, budget)?;
        }
    }
    match unsafe { NtDeleteKey(key.0) } {
        // Deleted concurrently: gone all the same.
        STATUS_KEY_DELETED => Ok(()),
        status if status < 0 => Err(unsafe { RtlNtStatusToDosError(status) }),
        _ => Ok(()),
    }
}

/// The names of the direct subkeys of `key`, at most `limit` of them.
fn subkey_names(key: &Key, limit: usize) -> Result<Vec<Vec<u16>>, u32> {
    let mut names = Vec::new();
    loop {
        // A key name has at most 255 characters.
        let mut buffer = [0u16; 256];
        let mut length = buffer.len() as u32;
        match unsafe {
            RegEnumKeyExW(
                key.0,
                names.len() as u32,
                buffer.as_mut_ptr(),
                &mut length,
                null(),
                null_mut(),
                null_mut(),
                null_mut(),
            )
        } {
            ERROR_SUCCESS => {}
            ERROR_NO_MORE_ITEMS => return Ok(names),
            code => return Err(code),
        }
        if names.len() == limit {
            return Err(ERROR_NOT_ENOUGH_QUOTA);
        }
        let name = buffer.get(..length as usize).ok_or(ERROR_INVALID_DATA)?;
        names.push(name.to_vec());
    }
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
            // Delete the link object by its handle, never recursively follow it
            // (a test may already have deleted it through `delete_tree`).
            let status = unsafe { NtDeleteKey(self.0 .0) };
            assert!(status == 0 || status == STATUS_KEY_DELETED, "{status:#x}");
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

    const MARKER: u32 = 0x5EED_F00D;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    fn set_marker(key: &Key) {
        let value = MARKER;
        let code = unsafe {
            RegSetValueExW(
                key.0,
                wide("Marker").as_ptr(),
                0,
                REG_DWORD,
                (&raw const value).cast(),
                4,
            )
        };
        assert_eq!(code, ERROR_SUCCESS);
    }

    /// The `Marker` value of the ordinary test key `path`.
    fn marker(path: &str) -> Option<u32> {
        let mut value = 0u32;
        let mut bytes = 4;
        let code = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                wide(path).as_ptr(),
                wide("Marker").as_ptr(),
                RRF_RT_REG_DWORD,
                null_mut(),
                (&raw mut value).cast(),
                &mut bytes,
            )
        };
        (code == ERROR_SUCCESS).then_some(value)
    }

    /// Whether the key `path` exists as itself (a link is not followed).
    fn exists_itself(path: &str) -> bool {
        let mut key = null_mut();
        let code = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                wide(path).as_ptr(),
                REG_OPTION_OPEN_LINK,
                KEY_QUERY_VALUE,
                &mut key,
            )
        };
        if code == ERROR_SUCCESS {
            drop(Key(key));
            return true;
        }
        assert_eq!(code, ERROR_FILE_NOT_FOUND, "{path}");
        false
    }

    #[test]
    fn delete_tree_removes_nested_subkeys_and_succeeds_when_missing() {
        let fixture = Fixture::new();
        set_marker(&fixture.key(r"FeatherTask\Preferences"));
        drop(fixture.key(r"FeatherTask\A\B\C"));
        drop(fixture.key(r"FeatherTask\A\Sibling"));
        set_marker(&fixture.key("Kept"));
        delete_tree(HKEY_CURRENT_USER, &fixture.path("FeatherTask")).unwrap();
        assert!(!exists_itself(&fixture.path("FeatherTask")));
        // Only that key: its sibling and parent remain.
        assert_eq!(marker(&fixture.path("Kept")), Some(MARKER));
        assert!(exists_itself(&fixture.path));
        // Already gone, or a missing ancestor: nothing to do.
        delete_tree(HKEY_CURRENT_USER, &fixture.path("FeatherTask")).unwrap();
        delete_tree(HKEY_CURRENT_USER, &fixture.path(r"Missing\FeatherTask")).unwrap();
        // An empty component is invalid, never the parent itself.
        assert_eq!(
            delete_tree(HKEY_CURRENT_USER, &fixture.path("")),
            Err(ERROR_INVALID_PARAMETER)
        );
        assert!(exists_itself(&fixture.path("Kept")));
    }

    #[test]
    fn delete_tree_removes_links_as_links_and_keeps_their_targets() {
        let fixture = Fixture::new();
        let target = fixture.key("UnrelatedTarget");
        set_marker(&target);
        set_marker(&fixture.key(r"UnrelatedTarget\Existing"));
        drop(fixture.key(r"FeatherTask\Preferences"));
        let _nested = fixture.link(r"FeatherTask\Preferences\Link", Some(&target));
        let _leaf = fixture.link(r"FeatherTask\Link", Some(&target));
        let _unfinished = fixture.link(r"FeatherTask\UnfinishedLink", None);
        // The links are live: an ordinary open reaches the target's subtree.
        assert_eq!(
            marker(&fixture.path(r"FeatherTask\Preferences\Link\Existing")),
            Some(MARKER)
        );
        delete_tree(HKEY_CURRENT_USER, &fixture.path("FeatherTask")).unwrap();
        assert!(!exists_itself(&fixture.path("FeatherTask")));
        // The removed key itself is a link.
        let _root = fixture.link("RootLink", Some(&target));
        assert_eq!(marker(&fixture.path("RootLink")), Some(MARKER));
        delete_tree(HKEY_CURRENT_USER, &fixture.path("RootLink")).unwrap();
        assert!(!exists_itself(&fixture.path("RootLink")));
        // A link among the ancestors fails the call.
        let _ancestor = fixture.link("AncestorLink", Some(&target));
        assert!(delete_tree(HKEY_CURRENT_USER, &fixture.path(r"AncestorLink\Existing")).is_err());
        assert!(exists_itself(&fixture.path("AncestorLink")));
        // The target, its value and its subkey were never touched.
        assert_eq!(marker(&fixture.path("UnrelatedTarget")), Some(MARKER));
        assert_eq!(
            marker(&fixture.path(r"UnrelatedTarget\Existing")),
            Some(MARKER)
        );
    }

    #[test]
    fn delete_tree_stops_at_its_depth_and_key_bounds() {
        let fixture = Fixture::new();
        drop(fixture.key(r"Deep\1\2\3"));
        assert_eq!(
            delete_tree_within(HKEY_CURRENT_USER, &fixture.path("Deep"), 2, 100),
            Err(ERROR_STACK_OVERFLOW)
        );
        assert!(exists_itself(&fixture.path(r"Deep\1\2\3")));
        delete_tree_within(HKEY_CURRENT_USER, &fixture.path("Deep"), 3, 100).unwrap();
        assert!(!exists_itself(&fixture.path("Deep")));
        for child in ["a", "b", "c", "d"] {
            drop(fixture.key(&format!(r"Wide\{child}")));
        }
        assert_eq!(
            delete_tree_within(HKEY_CURRENT_USER, &fixture.path("Wide"), 1, 4),
            Err(ERROR_NOT_ENOUGH_QUOTA)
        );
        assert!(exists_itself(&fixture.path(r"Wide\d")));
        delete_tree_within(HKEY_CURRENT_USER, &fixture.path("Wide"), 1, 5).unwrap();
        assert!(!exists_itself(&fixture.path("Wide")));
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
