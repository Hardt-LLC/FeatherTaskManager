//! Explicit process/service navigation. SCM and handle queries run only on
//! the job worker; the UI receives a checked process generation, never a PID
//! to act on later without its creation time.
use super::*;

pub(super) const FILE_PROPERTIES: usize = 516;
pub(super) const GO_TO_SERVICES: usize = 517;
pub(super) const GO_TO_PROCESS: usize = 518;

#[derive(Clone)]
pub(super) enum Request {
    Services(ProcessIdentity),
    Process(String),
}

pub(super) enum Target {
    Services(Vec<Service>, u32),
    Process(ProcessIdentity),
}

pub(super) fn resolve(request: &Request) -> Result<Target, String> {
    match request {
        Request::Services(identity) => {
            // Keep the source generation stable across the SCM lookup.
            let verify = || -> Result<(), String> {
                if crate::actions::running_process_created(identity.pid)? != identity.created {
                    return Err(tr(
                        "프로세스가 변경되었습니다. 목록을 새로 고치세요.",
                        "The process changed. Refresh the list.",
                    )
                    .into());
                }
                Ok(())
            };
            verify()?;
            let services = crate::services::list()?;
            verify()?;
            if !services.iter().any(|s| s.pid == identity.pid) {
                return Err(tr(
                    "이 프로세스에서 실행 중인 서비스가 없습니다.",
                    "No services are running in this process.",
                )
                .into());
            }
            Ok(Target::Services(services, identity.pid))
        }
        Request::Process(name) => {
            let pid = crate::services::process_id(name)?;
            if pid == 0 {
                return Err(tr(
                    "서비스가 현재 프로세스에서 실행되고 있지 않습니다.",
                    "This service has no running process.",
                )
                .into());
            }
            let created = crate::actions::running_process_created(pid)?;
            // A restart during the lookup must not point to the former host.
            if crate::services::process_id(name)? != pid
                || crate::actions::running_process_created(pid)? != created
            {
                return Err(tr(
                    "서비스 프로세스가 변경되었습니다. 다시 시도하세요.",
                    "The service process changed. Try again.",
                )
                .into());
            }
            Ok(Target::Process(ProcessIdentity { pid, created }))
        }
    }
}

pub(super) unsafe fn begin(p: *mut App, request: Request) {
    if (*p).busy || (*p).modal {
        return;
    }
    if (*p).jobs.send(Job::Navigate(request)).is_ok() {
        (*p).busy = true;
        update_buttons(p);
    }
}

pub(super) unsafe fn finish(p: *mut App, request: Request, result: Result<Target, String>) {
    (*p).busy = false;
    let source = match request {
        Request::Services(id) => Identity::Process(id.pid, id.created),
        Request::Process(name) => Identity::Service(name),
    };
    // A delayed result must not drag the user away from a new selection/page.
    if selected_identity(p).as_ref() != Some(&source) {
        return;
    }
    let target = match result {
        Ok(target) => target,
        Err(error) => {
            (*p).set_error(ErrorSource::Action, error);
            return;
        }
    };
    match target {
        Target::Services(services, pid) => {
            let selected = services
                .iter()
                .find(|s| s.pid == pid)
                .map(|s| Identity::Service(s.name.clone()));
            (*p).services = services;
            (*p).services_loaded = true;
            switch_page(p, Page::Services);
            SetWindowTextW((*p).search, wide(&format!("pid:{pid}")).as_ptr());
            rebuild(p, selected);
        }
        Target::Process(id) => {
            let exists = (*p).snapshot.as_ref().is_some_and(|s| {
                s.processes
                    .iter()
                    .any(|process| ProcessIdentity::from(process) == id)
            });
            if !exists {
                (*p).set_error(ErrorSource::Action, tr(
                    "프로세스가 아직 목록에 없거나 종료되었습니다. 새로 고친 후 다시 시도하세요.",
                    "The process is not in the current snapshot or has exited. Refresh and try again.",
                ).into());
                let _ = (*p).tx.send(Command::Refresh);
                return;
            }
            switch_page(p, Page::Processes);
            // A precise search expands the matching app/tree path without
            // changing the user's preferred grouped/list/tree mode.
            SetWindowTextW((*p).search, wide(&format!("pid:{}", id.pid)).as_ptr());
            rebuild(p, Some(Identity::Process(id.pid, id.created)));
        }
    }
    let row = SendMessageW(
        (*p).list,
        LVM_GETNEXTITEM,
        usize::MAX,
        LVNI_SELECTED as isize,
    );
    if row >= 0 {
        SendMessageW((*p).list, LVM_ENSUREVISIBLE, row as usize, 0);
    }
    SetFocus((*p).list);
}

/// Ordinary search retains its existing substring behavior. `pid:` is an
/// exact filter so PID 12 never also matches 120 or a service's display name.
pub(super) fn matches_search(filter: &str, pid: u32, names: &[&str]) -> bool {
    if let Some(value) = filter.strip_prefix("pid:") {
        return value.trim().parse::<u32>() == Ok(pid);
    }
    names
        .iter()
        .any(|name| name.to_lowercase().contains(filter))
        || pid.to_string().contains(filter)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_pid_search_does_not_match_reused_prefixes_or_names() {
        assert!(matches_search("pid:12", 12, &["service"]));
        assert!(!matches_search("pid:12", 120, &["pid:12 service"]));
        assert!(!matches_search("pid:", 0, &["service"]));
        assert!(!matches_search("pid:4294967296", 0, &["service"]));
        assert!(matches_search("audio", 120, &["AudioSrv", "Windows Audio"]));
        assert!(matches_search("12", 120, &["service"]));
    }
}
