//! Small preferences, read once and saved only on explicit changes.
use super::*;
use windows_sys::Win32::System::Registry::*;
const PATH: &str = r"Software\FeatherTask\Preferences";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Preferences {
    pub theme: u32,
    pub rate: u64,
    pub default_page: u32,
    pub topmost: bool,
    pub tray: bool,
    /// Ask for elevation (the UAC prompt) at every window launch
    /// (`elevation::relaunch`).
    pub always_admin: bool,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            theme: 0,
            rate: 1000,
            default_page: 0,
            topmost: false,
            tray: false,
            always_admin: false,
        }
    }
}
fn read(path: &str, name: &str) -> Option<u32> {
    let key = crate::registry::open(HKEY_CURRENT_USER, path, KEY_QUERY_VALUE).ok()??;
    let mut value = 0u32;
    let mut bytes = 4;
    let code = unsafe {
        RegGetValueW(
            key.0,
            null(),
            wide(name).as_ptr(),
            RRF_RT_REG_DWORD,
            null_mut(),
            (&mut value as *mut u32).cast(),
            &mut bytes,
        )
    };
    (code == 0 && bytes == 4).then_some(value)
}
impl Preferences {
    pub fn load() -> Self {
        Self::from_values(|name| read(PATH, name))
    }
    /// The preferences from their stored DWORD values (`read` by name);
    /// missing or invalid values keep the default, and a switch is on only
    /// when its value is exactly 1.
    fn from_values(read: impl Fn(&str) -> Option<u32>) -> Self {
        let mut p = Self::default();
        p.theme = read("Theme").filter(|v| *v <= 2).unwrap_or(p.theme);
        p.rate = read("RefreshMs")
            .filter(|v| [250, 500, 1000, 2000, 5000].contains(v))
            .map(u64::from)
            .unwrap_or(p.rate);
        p.default_page = read("StartPage").filter(|v| *v < 4).unwrap_or(0);
        p.topmost = read("Topmost") == Some(1);
        p.tray = read("MinimizeToTray") == Some(1);
        p.always_admin = read("AlwaysRunAsAdministrator") == Some(1);
        p
    }
    /// Every stored value, by name (the inverse of [`Self::from_values`]).
    fn values(&self) -> [(&'static str, u32); 6] {
        [
            ("Theme", self.theme),
            ("RefreshMs", self.rate as u32),
            ("StartPage", self.default_page),
            ("Topmost", self.topmost as u32),
            ("MinimizeToTray", self.tray as u32),
            ("AlwaysRunAsAdministrator", self.always_admin as u32),
        ]
    }
    /// The values that differ from `stored` (what this window last read or
    /// saved). Each window writes only its own changes, so another open
    /// window (such as the unelevated one "Run as administrator" leaves
    /// open) never reverts a value with its stale copy.
    fn changed_values(&self, stored: &Preferences) -> Vec<(&'static str, u32)> {
        self.values()
            .into_iter()
            .zip(stored.values())
            .filter(|(new, old)| new.1 != old.1)
            .map(|(new, _)| new)
            .collect()
    }
    /// Write the values changed since `stored` ([`Self::changed_values`]).
    pub fn save(&self, stored: &Preferences) -> Result<(), String> {
        let changes = self.changed_values(stored);
        if changes.is_empty() {
            return Ok(());
        }
        let key = crate::registry::create(HKEY_CURRENT_USER, PATH, KEY_SET_VALUE).map_err(|e| {
            format!(
                "{} (Windows {e})",
                tr("설정을 저장할 수 없습니다", "Cannot save preferences")
            )
        })?;
        for (name, value) in changes {
            let code = unsafe {
                RegSetValueExW(
                    key.0,
                    wide(name).as_ptr(),
                    0,
                    REG_DWORD,
                    (&value as *const u32).cast(),
                    4,
                )
            };
            if code != 0 {
                return Err(format!(
                    "{} (Windows {code})",
                    tr("설정을 저장할 수 없습니다", "Cannot save preferences")
                ));
            }
        }
        Ok(())
    }
    pub fn apply_theme(&self) {
        super::theme::set_dark(
            self.theme == 2
                || (self.theme == 0
                    && read(
                        r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
                        "AppsUseLightTheme",
                    ) == Some(0)),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Save to and load from an in-memory map: never the user's HKCU key.
    fn round_trip(prefs: &Preferences) -> Preferences {
        let stored: HashMap<&str, u32> = prefs.values().into_iter().collect();
        Preferences::from_values(|name| stored.get(name).copied())
    }

    #[test]
    fn preferences_round_trip_in_memory_including_always_run_as_administrator() {
        assert_eq!(round_trip(&Preferences::default()), Preferences::default());
        for always_admin in [false, true] {
            let prefs = Preferences {
                theme: 2,
                rate: 250,
                default_page: 3,
                topmost: true,
                tray: true,
                always_admin,
            };
            assert_eq!(round_trip(&prefs), prefs);
        }
        // Each value has its own name.
        let names: HashSet<&str> = Preferences::default()
            .values()
            .iter()
            .map(|(name, _)| *name)
            .collect();
        assert_eq!(names.len(), 6);
    }

    #[test]
    fn a_save_writes_only_the_values_this_window_changed() {
        let stored = Preferences::default();
        assert!(stored.changed_values(&stored).is_empty());
        // Another window turned "Always run as administrator" on; this one
        // (still holding the old value) changes only the theme.
        let theme = Preferences {
            theme: 2,
            ..stored.clone()
        };
        assert_eq!(theme.changed_values(&stored), vec![("Theme", 2)]);
        let admin = Preferences {
            always_admin: true,
            ..stored.clone()
        };
        assert_eq!(
            admin.changed_values(&stored),
            vec![("AlwaysRunAsAdministrator", 1)]
        );
        assert_eq!(
            stored.changed_values(&admin),
            vec![("AlwaysRunAsAdministrator", 0)]
        );
    }

    #[test]
    fn always_run_as_administrator_is_off_unless_stored_as_exactly_one() {
        for (stored, expected) in [
            (None, false),
            (Some(0), false),
            (Some(1), true),
            (Some(2), false),
            (Some(u32::MAX), false),
        ] {
            let prefs = Preferences::from_values(|name| {
                (name == "AlwaysRunAsAdministrator")
                    .then_some(stored)
                    .flatten()
            });
            assert_eq!(prefs.always_admin, expected, "{stored:?}");
            // The other values keep their defaults.
            assert_eq!(
                Preferences {
                    always_admin: false,
                    ..prefs
                },
                Preferences::default()
            );
        }
    }
}
