//! Small, allocation-free text selection. Preferences are read once, never polled.
use std::{
    ptr::{null, null_mut},
    sync::atomic::{AtomicU8, Ordering},
};
use windows_sys::Win32::{
    Foundation::ERROR_SUCCESS,
    Globalization::GetUserDefaultUILanguage,
    System::Registry::{
        RegGetValueW, RegSetValueExW, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ,
        RRF_RT_REG_SZ,
    },
};

const PREFERENCES: &str = "Software\\FeatherTask\\Preferences";
// Unit tests do not initialize preferences and retain the historical Korean default.
static LANGUAGE: AtomicU8 = AtomicU8::new(0);

#[cfg(test)]
thread_local! {
    static TEST_LANGUAGE: std::cell::Cell<Option<Language>> = const { std::cell::Cell::new(None) };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    Korean,
    English,
}

impl Language {
    pub fn code(self) -> &'static str {
        match self {
            Self::Korean => "ko",
            Self::English => "en",
        }
    }
    fn parse(value: &str) -> Option<Self> {
        match value {
            "ko" => Some(Self::Korean),
            "en" => Some(Self::English),
            _ => None,
        }
    }
}

pub fn language() -> Language {
    #[cfg(test)]
    if let Some(selected) = TEST_LANGUAGE.with(|value| value.get()) {
        return selected;
    }
    if LANGUAGE.load(Ordering::Relaxed) == 1 {
        Language::English
    } else {
        Language::Korean
    }
}

#[cfg(test)]
pub fn with_language<R>(selected: Language, run: impl FnOnce() -> R) -> R {
    struct Restore(Option<Language>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_LANGUAGE.with(|value| value.set(self.0));
        }
    }
    let _restore = Restore(TEST_LANGUAGE.with(|value| value.replace(Some(selected))));
    run()
}

pub fn tr<'a>(korean: &'a str, english: &'a str) -> &'a str {
    match language() {
        Language::Korean => korean,
        Language::English => english,
    }
}

#[macro_export]
macro_rules! trf {
    ($ko:literal, $en:literal $(, $argument:expr)* $(,)?) => {
        if $crate::i18n::language() == $crate::i18n::Language::English {
            format!($en $(, $argument)*)
        } else {
            format!($ko $(, $argument)*)
        }
    };
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

fn preferred_language() -> Option<Language> {
    preferred_language_at(PREFERENCES)
}

fn preferred_language_at(path: &str) -> Option<Language> {
    let key = crate::registry::open(HKEY_CURRENT_USER, path, KEY_QUERY_VALUE).ok()??;
    let mut buffer = [0_u16; 8];
    let mut bytes = std::mem::size_of_val(&buffer) as u32;
    let result = unsafe {
        RegGetValueW(
            key.0,
            null(),
            wide("Language").as_ptr(),
            RRF_RT_REG_SZ,
            null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    if result != ERROR_SUCCESS
        || !bytes.is_multiple_of(2)
        || bytes < 2
        || bytes as usize > std::mem::size_of_val(&buffer)
    {
        return None;
    }
    let units = &buffer[..bytes as usize / 2];
    if units.last() != Some(&0) {
        return None;
    }
    Language::parse(&String::from_utf16(&units[..units.len() - 1]).ok()?)
}

fn cli_language(args: &[String]) -> Option<Language> {
    // Everything after this flag belongs to the original Windows Task Manager.
    if args.get(1).map(String::as_str) == Some("--task-manager") {
        return None;
    }
    args.windows(2)
        .find(|pair| pair[0] == "--language")
        .and_then(|pair| Language::parse(&pair[1]))
}

pub fn initialize(args: &[String]) {
    let selected = cli_language(args)
        .or_else(preferred_language)
        .unwrap_or_else(|| {
            if unsafe { GetUserDefaultUILanguage() } & 0x03ff == 0x12 {
                Language::Korean
            } else {
                Language::English
            }
        });
    LANGUAGE.store(u8::from(selected == Language::English), Ordering::Relaxed);
}

pub fn set_language(selected: Language) -> Result<(), String> {
    save_language_at(PREFERENCES, selected)?;
    LANGUAGE.store(u8::from(selected == Language::English), Ordering::Relaxed);
    Ok(())
}

fn save_language_at(path: &str, selected: Language) -> Result<(), String> {
    let key = crate::registry::create(HKEY_CURRENT_USER, path, KEY_SET_VALUE)
        .map_err(preference_error)?;
    let value = wide(selected.code());
    let result = unsafe {
        RegSetValueExW(
            key.0,
            wide("Language").as_ptr(),
            0,
            REG_SZ,
            value.as_ptr().cast(),
            (value.len() * 2) as u32,
        )
    };
    if result != ERROR_SUCCESS {
        return Err(preference_error(result));
    }
    Ok(())
}

fn preference_error(code: u32) -> String {
    format!(
        "{}: {}",
        tr(
            "언어 설정을 저장하지 못했습니다",
            "Could not save the language preference"
        ),
        std::io::Error::from_raw_os_error(code as i32)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferences_reject_leaf_and_ancestor_links_without_changing_target() {
        let fixture = crate::registry::test_support::Fixture::new();
        let target = fixture.key("UnrelatedTarget");
        let _nested = fixture.key(r"UnrelatedTarget\Nested");
        save_language_at(&fixture.path("UnrelatedTarget"), Language::English).unwrap();
        save_language_at(&fixture.path(r"UnrelatedTarget\Nested"), Language::English).unwrap();
        let _link = fixture.link("PreferencesLink", Some(&target));
        let _unfinished = fixture.link("UnfinishedLink", None);
        for path in [
            "PreferencesLink",
            r"PreferencesLink\Nested",
            r"PreferencesLink\Missing",
            "UnfinishedLink",
        ] {
            assert_eq!(preferred_language_at(&fixture.path(path)), None);
            assert!(save_language_at(&fixture.path(path), Language::Korean).is_err());
        }
        assert_eq!(
            preferred_language_at(&fixture.path("UnrelatedTarget")),
            Some(Language::English)
        );
        assert_eq!(
            preferred_language_at(&fixture.path(r"UnrelatedTarget\Nested")),
            Some(Language::English)
        );
        assert!(crate::registry::open(
            HKEY_CURRENT_USER,
            &fixture.path(r"UnrelatedTarget\Missing"),
            KEY_QUERY_VALUE
        )
        .unwrap()
        .is_none());
        save_language_at(&fixture.path(r"Regular\Preferences"), Language::Korean).unwrap();
        assert_eq!(
            preferred_language_at(&fixture.path(r"Regular\Preferences")),
            Some(Language::Korean)
        );
    }

    #[test]
    fn command_line_language_is_explicit_and_ignores_ifeo_target_arguments() {
        let args = |values: &[&str]| values.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            cli_language(&args(&["app", "--language", "en"])),
            Some(Language::English)
        );
        assert_eq!(
            cli_language(&args(&["app", "--language", "ko"])),
            Some(Language::Korean)
        );
        assert_eq!(cli_language(&args(&["app", "--language", "unknown"])), None);
        assert_eq!(
            cli_language(&args(&[
                "app",
                "--task-manager",
                "taskmgr.exe",
                "--language",
                "en"
            ])),
            None
        );
    }
}
