//! Control groups, and the units systemd keeps in them.
//!
//! A process comes and goes; the unit it runs in has a name that outlives
//! it. `postgres[1234]` is a different series after a restart, and
//! `postgresql.service` is the same one.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

/// What kind of unit a control group is, by its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Service,
    Scope,
    Slice,
}

pub fn kind(component: &str) -> Option<Kind> {
    if component.ends_with(".service") {
        Some(Kind::Service)
    } else if component.ends_with(".scope") {
        Some(Kind::Scope)
    } else if component.ends_with(".slice") {
        Some(Kind::Slice)
    } else {
        None
    }
}

/// What a reported unit is, for the `kind` label: `service`, `scope`,
/// `slice`, or `manager`.
///
/// A slice's figures are the sum of the units in it, and so are those of
/// a user's manager, `user@1000.service`, which is a service by its name
/// and holds every unit that user runs. A reader who ranks units wants
/// neither among them, and can only ask for what a label equals.
pub fn unit_kind(unit: &str) -> &'static str {
    let name = unit.rsplit('/').next().unwrap_or(unit);
    match kind(name) {
        Some(Kind::Slice) => "slice",
        Some(Kind::Scope) => "scope",
        Some(Kind::Service) if name.starts_with("user@") => "manager",
        Some(Kind::Service) | None => "service",
    }
}

/// Whether a unit is made of other units. At the top of a list of units by
/// size it says what the rest of the list says again.
#[cfg(any(feature = "watch", test))]
pub fn is_sum(unit: &str) -> bool {
    matches!(unit_kind(unit), "slice" | "manager")
}

/// The control group of a process, from `/proc/<pid>/cgroup`: the path on
/// the unified hierarchy's line, `0::/system.slice/sshd.service`.
pub fn parse_proc_cgroup(text: &str) -> Option<&str> {
    text.lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(str::trim_end)
}

/// systemd writes the characters it cannot use in a name as `\xNN`.
pub fn unescape(name: &str) -> String {
    let bytes = name.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' && bytes.get(index + 1) == Some(&b'x') && index + 3 < bytes.len() {
            // Through the bytes: the two after `\x` need not be ASCII.
            let digits = std::str::from_utf8(&bytes[index + 2..index + 4]).ok();
            if let Some(value) = digits.and_then(|d| u8::from_str_radix(d, 16).ok()) {
                out.push(value);
                index += 4;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn all_digits(token: &str) -> bool {
    !token.is_empty() && token.bytes().all(|b| b.is_ascii_digit())
}

/// Lowercase hexadecimal with a digit in it: `2e5cb917`, and not `facade`.
fn random_hex(token: &str, at_least: usize) -> bool {
    token.len() >= at_least
        && token
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && token.bytes().any(|b| b.is_ascii_digit())
}

/// Whether an instance is one connection to a socket with `Accept=yes`,
/// which systemd names for a counter, the connection's addresses, and in
/// later versions a number between: `724573-192.168.92.10:9200-192.168.92.11:44430`,
/// `0-32774-127.0.0.1:39811-127.0.0.1:40200`. A health check through such a
/// socket is a new instance every second. A connection over a unix socket
/// is named for numbers alone, and is numbered already.
fn per_connection(instance: &str) -> bool {
    let parts: Vec<&str> = instance.split('-').collect();
    match parts.split_last_chunk::<2>() {
        Some((numbers, [local, remote])) => {
            !numbers.is_empty()
                && numbers.iter().all(|part| all_digits(part))
                && local.contains(':')
                && remote.contains(':')
        }
        None => false,
    }
}

/// The name of a unit without what marks one instance of it.
///
/// A desktop starts each application in a scope of its own, named for the
/// application and then for the instance: `app-Hyprland-chromium-2e5cb917`,
/// `session-2`, `run-p5134-i78156`. Under its full name every launch would
/// be a new set of series, and the application would never have a line.
/// Under the application's name, its instances are added together.
///
/// A service is named by whoever wrote its unit file, so less is assumed:
/// `postgresql-16.service` keeps its number, and only a suffix too long to
/// be a version is taken for an instance.
pub fn normalize(name: &str) -> String {
    let Some(kind) = kind(name) else {
        return name.to_string();
    };
    let (stem, suffix) = name.rsplit_once('.').expect("kind() saw a suffix");

    if let Some((template, instance)) = stem.split_once('@') {
        // The user manager's instance is a user, not a launch.
        let numbered = instance
            .split(['-', '_'])
            .all(|part| all_digits(part) || random_hex(part, 6));
        if template != "user" && (numbered || per_connection(instance)) {
            return format!("{template}@.{suffix}");
        }
        return name.to_string();
    }

    // systemd-run names what it starts for the process that asked.
    if stem.starts_with("run-") {
        return format!("run.{suffix}");
    }

    let marks_an_instance = |part: &str| match kind {
        // As short as the groups of a UUID, which a scope may be named
        // for: `tmux-spawn-4d429531-de50-44d1-ab31`.
        Kind::Scope => all_digits(part) || random_hex(part, 4),
        Kind::Service => (all_digits(part) && part.len() >= 6) || random_hex(part, 8),
        Kind::Slice => false,
    };
    let mut parts: Vec<&str> = stem.split('-').collect();
    while parts.last().is_some_and(|part| marks_an_instance(part)) {
        parts.pop();
    }
    if parts.is_empty() {
        // Named for nothing but the instance, as a container's health
        // check is named for the container's id: there is no name to keep.
        return format!("transient.{suffix}");
    }
    format!("{}.{suffix}", parts.join("-"))
}

/// The uid of the user manager a path runs under, if it runs under one:
/// `/user.slice/user-1000.slice/user@1000.service/app.slice/x.service`.
fn manager_uid<'a>(components: impl Iterator<Item = &'a str>) -> Option<u32> {
    components
        .filter_map(|c| {
            c.strip_prefix("user@")?
                .strip_suffix(".service")?
                .parse()
                .ok()
        })
        .next()
}

/// A container's id, if a control group is named for one: sixty-four
/// hexadecimal digits, somewhere in its name.
fn container_id(component: &str) -> Option<&str> {
    component.split(['-', '.']).find(|part| {
        part.len() == 64
            && part
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// Containers, by id: the control group of the unit each one runs in.
///
/// A container runtime names what it makes for a container's id, which
/// says nothing to a reader and is different after every restart. Where a
/// container is run by a systemd unit, the runtime puts the container's
/// own group inside the unit's, named for the id:
///
/// ```text
/// caddy.service/libpod-payload-0dc2962b9682…
/// ```
///
/// and that is enough to give the id a name without asking the runtime.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Containers(HashMap<String, String>);

impl Containers {
    /// Read the control group tree under `base` for containers.
    pub fn scan(base: &Path) -> Self {
        let mut found = HashMap::new();
        Self::walk(base, "", None, &mut found);
        Self(found)
    }

    fn walk(dir: &Path, path: &str, unit: Option<&str>, found: &mut HashMap<String, String>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let Some(component) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let child = format!("{path}/{component}");
            match kind(&component) {
                Some(Kind::Service | Kind::Scope) => {
                    Self::walk(&entry.path(), &child, Some(&child), found);
                }
                Some(Kind::Slice) => Self::walk(&entry.path(), &child, unit, found),
                None => {
                    // A group inside a unit, named for a container: the
                    // unit is what runs the container.
                    if let (Some(id), Some(unit)) = (container_id(&component), unit) {
                        found.insert(id.to_string(), unit.to_string());
                    }
                    Self::walk(&entry.path(), &child, unit, found);
                }
            }
        }
    }

    /// The control group of the unit that runs the container a group is
    /// named for.
    fn owner(&self, component: &str) -> Option<&str> {
        self.0.get(container_id(component)?).map(String::as_str)
    }

    #[cfg(test)]
    pub fn of(pairs: &[(&str, &str)]) -> Self {
        Self(
            pairs
                .iter()
                .map(|(id, unit)| (id.to_string(), unit.to_string()))
                .collect(),
        )
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

/// The name a control group is reported under, if it is a unit.
///
/// A unit of the system keeps its name. A unit of a user's manager is
/// prefixed with the user, because both managers can have a `dbus.service`
/// and a label value has to name one line. A unit named for a container's
/// id is reported as the unit that runs the container: a health check is
/// part of what a container costs.
pub fn unit_name(
    path: &str,
    user_of: &mut dyn FnMut(u32) -> String,
    containers: &Containers,
) -> Option<String> {
    let components: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    let (last, ancestors) = components.split_last()?;
    kind(last)?;
    if let Some(owner) = containers.owner(last).filter(|owner| *owner != path) {
        // The owner is named for itself, whatever it is called.
        return unit_name(owner, user_of, &Containers::default());
    }
    let name = normalize(&unescape(last));
    Some(match manager_uid(ancestors.iter().copied()) {
        Some(uid) => format!("{}/{name}", user_of(uid)),
        None => name,
    })
}

/// The unit a process belongs to: the nearest service or scope at or above
/// its control group. Slices group units and are not one themselves; a
/// process directly in one (or in the root, as kernel threads are) belongs
/// to no unit.
pub fn unit_of(
    path: &str,
    user_of: &mut dyn FnMut(u32) -> String,
    containers: &Containers,
) -> Option<String> {
    let mut end = path.len();
    loop {
        let prefix = &path[..end];
        let last = prefix.rsplit('/').next().unwrap_or("");
        if matches!(kind(last), Some(Kind::Service | Kind::Scope)) {
            return unit_name(prefix, user_of, containers);
        }
        end = prefix.rfind('/')?;
        if end == 0 {
            return None;
        }
    }
}

/// Bytes read and written, by device, from a control group's `io.stat`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceIo {
    /// `major:minor`.
    pub device: String,
    pub read_bytes: u64,
    pub write_bytes: u64,
}

pub fn parse_io_stat(text: &str) -> Vec<DeviceIo> {
    let mut devices = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_ascii_whitespace();
        let Some(device) = fields.next() else {
            continue;
        };
        let mut io = DeviceIo {
            device: device.to_string(),
            read_bytes: 0,
            write_bytes: 0,
        };
        for field in fields {
            match field.split_once('=') {
                Some(("rbytes", value)) => io.read_bytes = value.parse().unwrap_or(0),
                Some(("wbytes", value)) => io.write_bytes = value.parse().unwrap_or(0),
                _ => {}
            }
        }
        devices.push(io);
    }
    devices
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_unit_is_of_one_kind() {
        assert_eq!(unit_kind("sshd.service"), "service");
        assert_eq!(unit_kind("getty@tty1.service"), "service");
        assert_eq!(unit_kind("mark/app-term.scope"), "scope");
        assert_eq!(unit_kind("system.slice"), "slice");
        assert_eq!(unit_kind("mark/app-graphical.slice"), "slice");
        assert_eq!(unit_kind("user@1000.service"), "manager");
        // By its name, and not by a word in it.
        assert_eq!(unit_kind("mark/slice-of-life.service"), "service");

        for unit in ["user.slice", "mark/app.slice", "user@1000.service"] {
            assert!(is_sum(unit), "{unit}");
        }
        for unit in ["getty@tty1.service", "mark/app-term.scope"] {
            assert!(!is_sum(unit), "{unit}");
        }
    }

    fn named(uid: u32) -> String {
        match uid {
            1000 => "mark".into(),
            other => other.to_string(),
        }
    }

    #[test]
    fn the_unified_hierarchy_is_the_line_that_starts_with_zero() {
        assert_eq!(
            parse_proc_cgroup("0::/system.slice/sshd.service\n"),
            Some("/system.slice/sshd.service")
        );
        // A host still carrying version 1 hierarchies lists them first.
        assert_eq!(
            parse_proc_cgroup("12:pids:/user.slice\n1:name=systemd:/x\n0::/init.scope\n"),
            Some("/init.scope")
        );
        assert_eq!(parse_proc_cgroup("0::/\n"), Some("/"));
        assert_eq!(parse_proc_cgroup("1:cpu:/only/version/one\n"), None);
    }

    #[test]
    fn escaped_characters_are_restored() {
        assert_eq!(
            unescape(r"app-Hyprland-xdg\x2dterminal\x2dexec-805dd936.scope"),
            "app-Hyprland-xdg-terminal-exec-805dd936.scope"
        );
        assert_eq!(
            unescape(r"dev-disk-by\x2duuid.mount"),
            "dev-disk-by-uuid.mount"
        );
        // What is not an escape is left as it is.
        assert_eq!(unescape(r"odd\name\x"), r"odd\name\x");
        assert_eq!(unescape(r"bad\xzz"), r"bad\xzz");
        assert_eq!(unescape("caf\\xaé.scope"), "caf\\xaé.scope");
    }

    #[test]
    fn instances_of_a_scope_share_the_name_of_what_they_run() {
        for (full, shared) in [
            (
                "app-Hyprland-chromium-2e5cb917.scope",
                "app-Hyprland-chromium.scope",
            ),
            (
                "app-Hyprland-xdg-terminal-exec-805dd936.scope",
                "app-Hyprland-xdg-terminal-exec.scope",
            ),
            (
                "app-org.chromium.Chromium-10057.scope",
                "app-org.chromium.Chromium.scope",
            ),
            ("session-2.scope", "session.scope"),
            ("run-p5134-i78156.scope", "run.scope"),
            ("run-r0a1b2c3d4e5f.scope", "run.scope"),
            ("podman-pause-3d579928.scope", "podman-pause.scope"),
            ("app-editor-4711-0a1b2c3d.scope", "app-editor.scope"),
            (
                "tmux-spawn-4d429531-de50-44d1-ab31.scope",
                "tmux-spawn.scope",
            ),
            // A word that happens to be hexadecimal is still a word.
            ("app-cafe-beef.scope", "app-cafe-beef.scope"),
            ("0a1b2c3d4e5f.scope", "transient.scope"),
            // A container started by hand, named for its id by its runtime.
            (
                "libpod-3f9a1c07e5b24d68a0c1f2e3d4b5a69788776655443322110ffeeddccbbaa991.scope",
                "libpod.scope",
            ),
            (
                "docker-3f9a1c07e5b24d68a0c1f2e3d4b5a69788776655443322110ffeeddccbbaa991.scope",
                "docker.scope",
            ),
            ("init.scope", "init.scope"),
        ] {
            assert_eq!(normalize(full), shared, "{full}");
        }
    }

    #[test]
    fn a_service_keeps_the_name_its_author_gave_it() {
        for name in [
            "postgresql.service",
            "postgresql-16.service",
            "timeless-stack.service",
            "systemd-journald.service",
            "getty@tty1.service",
            "wayland-wm@hyprland.desktop.service",
            "user@1000.service",
            // Instances named for something other than a connection.
            "systemd-fsck@dev-disk-by\\x2duuid-0a1b.service",
            "openvpn-client@site-a.service",
            "container-getty@1:2.service",
            // A word that happens to be hexadecimal is still a word.
            "serve-facade.service",
        ] {
            assert_eq!(normalize(name), name);
        }
    }

    #[test]
    fn a_service_started_once_per_event_is_named_for_its_template() {
        for (full, shared) in [
            (
                "systemd-coredump@12-3456-0.service",
                "systemd-coredump@.service",
            ),
            (
                "wayland-session-bindpid@1374.service",
                "wayland-session-bindpid@.service",
            ),
            (
                "systemd-coredump@2-12289-131877_138086-0.service",
                "systemd-coredump@.service",
            ),
            (
                "omarchy-browser-1790704088432165608.service",
                "omarchy-browser.service",
            ),
            // A connection to a socket with Accept=yes, over TCP and IPv6,
            // and over a unix socket.
            (
                "galera-clustercheck@724573-192.168.92.10:9200-192.168.92.11:44430.service",
                "galera-clustercheck@.service",
            ),
            (
                "sshd@3-[2001:db8::1]:22-[2001:db8::2]:51234.service",
                "sshd@.service",
            ),
            (
                "tacct-acceptest@0-32774-127.0.0.1:39811-127.0.0.1:40200.service",
                "tacct-acceptest@.service",
            ),
            ("varlink@17-4321-0.service", "varlink@.service"),
            // A container's health check, named for the container's id.
            (
                "3f9a1c07e5b24d68a0c1f2e3d4b5a69788776655443322110ffeeddccbbaa991-1f2e3d4c5b6a7980.service",
                "transient.service",
            ),
        ] {
            assert_eq!(normalize(full), shared, "{full}");
        }
    }

    #[test]
    fn slices_are_never_renamed() {
        assert_eq!(normalize("user-1000.slice"), "user-1000.slice");
        assert_eq!(normalize("system-getty.slice"), "system-getty.slice");
    }

    #[test]
    fn a_unit_of_a_users_manager_is_named_for_the_user() {
        let user = "/user.slice/user-1000.slice/user@1000.service";
        let mut users = named;
        let none = Containers::default();
        assert_eq!(
            unit_name("/system.slice/caddy.service", &mut users, &none).as_deref(),
            Some("caddy.service")
        );
        assert_eq!(
            unit_name(
                &format!("{user}/app.slice/caddy.service"),
                &mut users,
                &none
            )
            .as_deref(),
            Some("mark/caddy.service")
        );
        assert_eq!(
            unit_name(&format!("{user}/app.slice"), &mut users, &none).as_deref(),
            Some("mark/app.slice")
        );
        // The manager itself belongs to the system.
        assert_eq!(
            unit_name(user, &mut users, &none).as_deref(),
            Some("user@1000.service")
        );
        assert_eq!(
            unit_name(
                &format!(r"{user}/app.slice/app-graphical.slice/app-Hyprland-xdg\x2dterminal\x2dexec-97b892fb.scope"),
                &mut users,
                &none
            )
            .as_deref(),
            Some("mark/app-Hyprland-xdg-terminal-exec.scope")
        );
        // A container's inner groups are not units.
        assert_eq!(
            unit_name(
                &format!("{user}/app.slice/caddy.service/libpod-payload-0dc2"),
                &mut users,
                &none
            ),
            None
        );
        assert_eq!(unit_name("/", &mut users, &none), None);
    }

    #[test]
    fn a_process_belongs_to_the_nearest_service_or_scope_above_it() {
        let user = "/user.slice/user-1000.slice/user@1000.service";
        let mut users = named;
        let none = Containers::default();
        assert_eq!(
            unit_of(
                &format!("{user}/app.slice/caddy.service/libpod-payload-0dc2"),
                &mut users,
                &none
            )
            .as_deref(),
            Some("mark/caddy.service")
        );
        assert_eq!(
            unit_of(
                "/system.slice/systemd-udevd.service/udev",
                &mut users,
                &none
            )
            .as_deref(),
            Some("systemd-udevd.service")
        );
        assert_eq!(
            unit_of(
                "/user.slice/user-1000.slice/session-2.scope",
                &mut users,
                &none
            )
            .as_deref(),
            Some("session.scope")
        );
        assert_eq!(
            unit_of("/init.scope", &mut users, &none).as_deref(),
            Some("init.scope")
        );
        // Kernel threads live in the root, and a slice is not a unit to
        // belong to.
        assert_eq!(unit_of("/", &mut users, &none), None);
        assert_eq!(unit_of("/system.slice", &mut users, &none), None);
        assert_eq!(unit_of("", &mut users, &none), None);
    }

    const ID: &str = "3f9a1c07e5b24d68a0c1f2e3d4b5a69788776655443322110ffeeddccbbaa991";

    #[test]
    fn a_unit_named_for_a_container_is_reported_as_what_runs_the_container() {
        let user = "/user.slice/user-1000.slice/user@1000.service/app.slice";
        let stack = format!("{user}/timeless-stack.service");
        let containers = Containers::of(&[(ID, &stack)]);
        let mut users = named;

        // Its health check, in a unit of its own beside it.
        let check = format!("{user}/{ID}-1f2e3d4c5b6a7980.service");
        assert_eq!(
            unit_name(&check, &mut users, &containers).as_deref(),
            Some("mark/timeless-stack.service")
        );
        assert_eq!(
            unit_of(&format!("{check}/probe"), &mut users, &containers).as_deref(),
            Some("mark/timeless-stack.service")
        );
        // The unit itself, and what is inside it, are as they were.
        assert_eq!(
            unit_of(
                &format!("{stack}/libpod-payload-{ID}"),
                &mut users,
                &containers
            )
            .as_deref(),
            Some("mark/timeless-stack.service")
        );
        // A container nothing is known to run keeps the name of no one.
        assert_eq!(
            unit_name(&check, &mut users, &Containers::default()).as_deref(),
            Some("mark/transient.service")
        );
        let other = "0".repeat(64);
        assert_eq!(
            unit_name(
                &format!("{user}/{other}-1f2e3d4c5b6a7980.service"),
                &mut users,
                &containers
            )
            .as_deref(),
            Some("mark/transient.service")
        );
    }

    #[test]
    fn containers_are_found_in_the_units_that_run_them() {
        let fixture = crate::testutil::Fixture::new("cgroup_containers");
        let app = "fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice";
        fixture.sys_dir(&format!("{app}/timeless-stack.service/libpod-payload-{ID}"));
        fixture.sys_dir(&format!("{app}/timeless-stack.service/runtime"));
        fixture.sys_dir(&format!("{app}/caddy.service"));
        // A unit named for a container does not run it.
        fixture.sys_dir(&format!("{app}/{ID}-1f2e3d4c5b6a7980.service"));
        // Nor does a slice.
        fixture.sys_dir(&format!(
            "fs/cgroup/machine.slice/libpod-{}",
            "1".repeat(64)
        ));

        let containers = Containers::scan(&fixture.path("sys/fs/cgroup"));
        assert_eq!(containers.len(), 1);
        assert_eq!(
            containers.owner(&format!("{ID}-1f2e3d4c5b6a7980.service")),
            Some("/user.slice/user-1000.slice/user@1000.service/app.slice/timeless-stack.service")
        );
        assert_eq!(containers.owner("caddy.service"), None);
    }

    #[test]
    fn io_is_read_by_device() {
        let devices = parse_io_stat(
            "259:0 rbytes=10436608 wbytes=884736 rios=177 wios=96 dbytes=0 dios=0\n\
             253:0 rbytes=10436608 wbytes=884736 rios=177 wios=96\n\
             8:16\n",
        );
        assert_eq!(devices.len(), 3);
        assert_eq!(devices[0].device, "259:0");
        assert_eq!(devices[0].read_bytes, 10_436_608);
        assert_eq!(devices[0].write_bytes, 884_736);
        assert_eq!(devices[2].read_bytes, 0);
    }
}
