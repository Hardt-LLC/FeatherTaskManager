//! Small preferences, read once and saved only on explicit changes.
use super::*;
use windows_sys::Win32::System::Registry::*;
const PATH: &str = r"Software\FeatherTask\Preferences";

#[derive(Clone, Debug)]
pub(super) struct Preferences {
    pub theme: u32,
    pub rate: u64,
    pub default_page: u32,
    pub topmost: bool,
    pub tray: bool,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            theme: 0,
            rate: 1000,
            default_page: 0,
            topmost: false,
            tray: false,
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
        let mut p = Self::default();
        p.theme = read(PATH, "Theme").filter(|v| *v <= 2).unwrap_or(p.theme);
        p.rate = read(PATH, "RefreshMs")
            .filter(|v| [250, 500, 1000, 2000, 5000].contains(v))
            .map(u64::from)
            .unwrap_or(p.rate);
        p.default_page = read(PATH, "StartPage").filter(|v| *v < 4).unwrap_or(0);
        p.topmost = read(PATH, "Topmost") == Some(1);
        p.tray = read(PATH, "MinimizeToTray") == Some(1);
        p
    }
    pub fn save(&self) -> Result<(), String> {
        let key = crate::registry::create(HKEY_CURRENT_USER, PATH, KEY_SET_VALUE).map_err(|e| {
            format!(
                "{} (Windows {e})",
                tr("설정을 저장할 수 없습니다", "Cannot save preferences")
            )
        })?;
        for (name, value) in [
            ("Theme", self.theme),
            ("RefreshMs", self.rate as u32),
            ("StartPage", self.default_page),
            ("Topmost", self.topmost as u32),
            ("MinimizeToTray", self.tray as u32),
        ] {
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
