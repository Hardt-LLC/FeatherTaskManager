//! "Always run as administrator" (Settings → Window). At a window launch
//! with the setting on and an unelevated token, Feather asks Windows to start
//! the protected installed copy of this same build through the normal UAC
//! consent prompt and exits once the elevated instance has started. A
//! portable or different copy is never elevated automatically (like the
//! administrator helpers, `replacement::relaunch_installed_elevated`); it
//! keeps running unelevated with a notice, as does a declined or failed
//! prompt. There is no scheduled task, service or other way around the prompt.
use super::*;

/// The page names `--page` takes (`initial_page`; "processes" is its default).
const PAGES: [&str; 4] = ["processes", "performance", "startup", "services"];

/// The arguments of the elevated relaunch for a launch with `args` (argv,
/// the executable first), or None when this launch must not relaunch: the
/// setting is off, the token is already elevated, this is itself a relaunch
/// (`actions::ELEVATED_RELAUNCH`), or `args` is anything but a plain window
/// launch. Command-line, diagnostic and helper modes (`--self-test`,
/// `--render-previews`, `--purge-memory-lists`, …), unknown or repeated
/// options and invalid values never relaunch.
///
/// The forwarded arguments are rebuilt from fixed strings, never copied from
/// `args`: `--task-manager` alone (IFEO appends taskmgr.exe and Windows'
/// own arguments, which are dropped), else the known `--page` and the
/// display language, then the marker.
pub(super) fn relaunch_arguments(
    args: &[String],
    always_admin: bool,
    elevated: bool,
    language: Language,
) -> Option<Vec<&'static str>> {
    if !always_admin
        || elevated
        || args
            .iter()
            .any(|argument| argument == crate::actions::ELEVATED_RELAUNCH)
    {
        return None;
    }
    let options = args.get(1..).unwrap_or_default();
    let mut forwarded = Vec::with_capacity(6);
    if options.first().map(String::as_str) == Some("--task-manager") {
        forwarded.push("--task-manager");
    } else {
        let mut page = None;
        let mut language_given = false;
        for pair in options.chunks(2) {
            match pair {
                [flag, value] if flag == "--page" && page.is_none() => {
                    page = Some(PAGES.into_iter().find(|&known| known == value.as_str())?);
                }
                [flag, value]
                    if flag == "--language"
                        && !language_given
                        && matches!(value.as_str(), "ko" | "en") =>
                {
                    language_given = true;
                }
                _ => return None,
            }
        }
        if let Some(page) = page {
            forwarded.extend(["--page", page]);
        }
        // The language this instance resolved (the given one, else the
        // saved or Windows one), like "Run as administrator".
        forwarded.extend(["--language", language.code()]);
    }
    forwarded.push(crate::actions::ELEVATED_RELAUNCH);
    Some(forwarded)
}

/// Whether this instance is an elevated relaunch (the marker) that Windows
/// started without elevation, as "runas" does for a standard user with UAC
/// turned off: it says so instead of launching again.
pub(super) fn relaunch_left_unelevated(args: &[String], elevated: bool) -> bool {
    !elevated
        && args
            .iter()
            .any(|argument| argument == crate::actions::ELEVATED_RELAUNCH)
}

/// How a window launch continues after [`relaunch`].
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Startup {
    /// No relaunch was due: run this instance.
    Continue,
    /// The elevated instance started: exit without opening a window.
    Relaunched,
    /// No elevated instance (declined, not the installed copy, failed, or
    /// Windows left the relaunch unelevated): run this instance unelevated,
    /// with this status-bar notice (no retry).
    Unelevated(String),
}

/// Relaunch elevated when "Always run as administrator" asks for it.
pub(super) fn relaunch(args: &[String], prefs: &Preferences) -> Startup {
    let elevated = crate::netetw::is_elevated();
    if relaunch_left_unelevated(args, elevated) {
        return Startup::Unelevated(
            tr(
                "Windows가 관리자 권한 없이 Feather를 시작했습니다.",
                "Windows started Feather without administrator rights.",
            )
            .into(),
        );
    }
    let Some(arguments) = relaunch_arguments(args, prefs.always_admin, elevated, language()) else {
        return Startup::Continue;
    };
    match crate::replacement::relaunch_installed_elevated(&arguments) {
        Ok(crate::replacement::Relaunch::Started) => Startup::Relaunched,
        Ok(crate::replacement::Relaunch::Cancelled) => Startup::Unelevated(
            tr(
                "관리자 권한 요청이 거부되어 관리자 권한 없이 실행 중입니다.",
                "The administrator request was declined. Running without administrator rights.",
            )
            .into(),
        ),
        Err(reason) => Startup::Unelevated(tf!(
            "관리자 권한 없이 실행 중입니다: {}",
            "Running without administrator rights: {}",
            reason
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::ELEVATED_RELAUNCH as MARKER;

    fn args(values: &[&str]) -> Vec<String> {
        std::iter::once("Feather.exe")
            .chain(values.iter().copied())
            .map(str::to_owned)
            .collect()
    }
    fn decide(values: &[&str], always_admin: bool, elevated: bool) -> Option<Vec<&'static str>> {
        relaunch_arguments(&args(values), always_admin, elevated, Language::English)
    }

    #[test]
    fn relaunch_only_for_an_unelevated_window_launch_with_the_setting_on() {
        let plain = Some(vec!["--language", "en", MARKER]);
        assert_eq!(decide(&[], true, false), plain);
        // Setting off, or already elevated: never.
        for (always_admin, elevated) in [(false, false), (false, true), (true, true)] {
            assert_eq!(decide(&[], always_admin, elevated), None);
            assert_eq!(
                decide(&["--page", "services"], always_admin, elevated),
                None
            );
            assert_eq!(decide(&["--task-manager"], always_admin, elevated), None);
        }
        // The relaunched instance (marker anywhere) never relaunches again,
        // even when a "runas" start left it unelevated.
        for values in [
            &[MARKER][..],
            &["--language", "en", MARKER],
            &["--page", "performance", "--language", "ko", MARKER],
            &["--task-manager", MARKER],
            &[
                "--task-manager",
                "C:\\Windows\\System32\\taskmgr.exe",
                MARKER,
            ],
        ] {
            assert_eq!(decide(values, true, false), None, "{values:?}");
        }
    }

    #[test]
    fn a_relaunch_left_unelevated_says_so_instead_of_relaunching() {
        for values in [
            &[MARKER][..],
            &["--language", "en", MARKER],
            &["--page", "services", "--language", "ko", MARKER],
            &["--task-manager", MARKER],
        ] {
            assert!(relaunch_left_unelevated(&args(values), false), "{values:?}");
            assert!(!relaunch_left_unelevated(&args(values), true), "{values:?}");
            assert_eq!(decide(values, true, false), None, "{values:?}");
        }
        // No marker: an ordinary launch, elevated or not.
        for values in [&[][..], &["--page", "services"], &["--task-manager"]] {
            for elevated in [false, true] {
                assert!(!relaunch_left_unelevated(&args(values), elevated));
            }
        }
    }

    #[test]
    fn command_line_diagnostic_and_helper_modes_never_relaunch() {
        for values in [
            &["--self-test"][..],
            &["--self-test", "out.txt"],
            &["--render-previews"],
            &["--render-previews", "previews"],
            &["--dump-hardware", "hw.txt"],
            &["--memory-cleanup-dry-run", "dry.txt"],
            &["--purge-memory-lists", "all"],
            &["--purge-memory-lists", "standby", "--language", "en"],
            &["--install-task-manager"],
            &["--restore-task-manager", "--language", "ko"],
            &["--prepare-install-directory"],
            &["--validate-installation"],
            &["--remove-user-preferences"],
            &["--test-child"],
            // A mode after a valid option, or an unknown argument.
            &["--language", "en", "--self-test"],
            &["--page", "performance", "--test-child"],
            &["unexpected"],
            &["--unknown", "value"],
        ] {
            assert_eq!(decide(values, true, false), None, "{values:?}");
        }
    }

    #[test]
    fn only_known_options_are_forwarded_rebuilt_from_fixed_strings() {
        for page in PAGES {
            assert_eq!(
                decide(&["--page", page], true, false),
                Some(vec!["--page", page, "--language", "en", MARKER])
            );
        }
        // The resolved display language is forwarded, not the argument text.
        assert_eq!(
            relaunch_arguments(
                &args(&["--language", "en", "--page", "startup"]),
                true,
                false,
                Language::Korean
            ),
            Some(vec!["--page", "startup", "--language", "ko", MARKER])
        );
        assert_eq!(
            relaunch_arguments(&args(&[]), true, false, Language::Korean),
            Some(vec!["--language", "ko", MARKER])
        );
        // IFEO (Ctrl+Shift+Esc): only the flag; Windows' taskmgr.exe path and
        // its arguments (which may look like Feather options) are dropped.
        for values in [
            &["--task-manager"][..],
            &["--task-manager", "C:\\Windows\\System32\\taskmgr.exe"],
            &[
                "--task-manager",
                "C:\\Windows\\System32\\taskmgr.exe",
                "/7",
                "--page",
                "services",
                "--self-test",
            ],
        ] {
            assert_eq!(
                decide(values, true, false),
                Some(vec!["--task-manager", MARKER]),
                "{values:?}"
            );
        }
        // Invalid, incomplete or repeated options: no relaunch at all.
        for values in [
            &["--page"][..],
            &["--page", "settings"],
            &["--page", "Performance"],
            &["--page", "performance extra"],
            &["--page", "\"performance\" --self-test"],
            &["--page", "services", "--page", "startup"],
            &["--language"],
            &["--language", "fr"],
            &["--language", "en", "--language", "ko"],
            &["--language", "en", "--task-manager"],
            &["--page", "services", "trailing"],
        ] {
            assert_eq!(decide(values, true, false), None, "{values:?}");
        }
        // Every forwarded token is one plain word (quoted nowhere).
        for values in [&[][..], &["--page", "services"], &["--task-manager", "x y"]] {
            for token in decide(values, true, false).unwrap() {
                assert!(!token.is_empty() && !token.contains([' ', '"', '\t']));
            }
        }
    }

    #[test]
    fn forwarded_arguments_open_the_same_page_and_language_elevated() {
        let relaunched = |values: &[&str], language| {
            let forwarded = relaunch_arguments(&args(values), true, false, language).unwrap();
            std::iter::once("Feather.exe".to_owned())
                .chain(forwarded.into_iter().map(str::to_owned))
                .collect::<Vec<_>>()
        };
        for (values, page) in [
            (&["--page", "performance"][..], Page::Performance),
            (&["--page", "startup"], Page::Startup),
            (&["--page", "services"], Page::Services),
            (&["--page", "processes"], Page::Processes),
            (
                &["--task-manager", "taskmgr.exe", "--page", "services"],
                Page::Processes,
            ),
        ] {
            assert_eq!(initial_page(&args(values)), page, "{values:?}");
            assert_eq!(
                initial_page(&relaunched(values, Language::English)),
                page,
                "{values:?}"
            );
        }
        // A plain launch opens the saved start page in both instances.
        assert!(!relaunched(&[], Language::English)
            .iter()
            .any(|a| a == "--page" || a == "--task-manager"));
    }
}
