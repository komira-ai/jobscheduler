//! Readers of the machine's system-wide views: `ipcs` for System V objects by owner,
//! `ps` for processes by uid, and the text of `launchctl print user/<uid>`.

use super::token::{Token, find_tokens};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpcKind {
    Message,
    Shared,
    Semaphore,
}

/// One row of `ipcs -a`: `T ID KEY MODE OWNER GROUP`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IpcObject {
    pub kind: IpcKind,
    pub id: u64,
    pub key: String,
    pub owner: String,
}

/// The objects `ipcs -a` lists, from its three tables. A heading, a header row, a blank
/// line or a line of another shape is skipped.
pub fn parse_ipcs(text: &str) -> Vec<IpcObject> {
    text.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 6 {
                return None;
            }
            let kind = match fields[0] {
                "q" => IpcKind::Message,
                "m" => IpcKind::Shared,
                "s" => IpcKind::Semaphore,
                _ => return None,
            };
            Some(IpcObject {
                kind,
                id: fields[1].parse().ok()?,
                key: fields[2].to_owned(),
                owner: fields[4].to_owned(),
            })
        })
        .collect()
}

/// The objects owned by `uid`: `ipcs` prints the owner as a user name, or as the number
/// when no user has the uid (the case after a delete), so both spellings count, and so
/// do the `names` given.
pub fn ipc_owned_by<'a>(objects: &'a [IpcObject], uid: u32, names: &[&str]) -> Vec<&'a IpcObject> {
    let number = uid.to_string();
    objects
        .iter()
        .filter(|object| object.owner == number || names.contains(&object.owner.as_str()))
        .collect()
}

/// One row of `ps -A -o uid=,pid=,command=`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proc {
    pub uid: u32,
    pub pid: u32,
    pub command: String,
}

pub fn parse_ps(text: &str) -> Vec<Proc> {
    text.lines()
        .filter_map(|line| {
            let (uid, rest) = line.trim_start().split_once(char::is_whitespace)?;
            let (pid, command) = rest.trim_start().split_once(char::is_whitespace)?;
            Some(Proc {
                uid: uid.parse().ok()?,
                pid: pid.parse().ok()?,
                command: command.trim().to_owned(),
            })
        })
        .collect()
}

pub fn procs_of_uid(procs: &[Proc], uid: u32) -> Vec<&Proc> {
    procs.iter().filter(|proc| proc.uid == uid).collect()
}

/// The processes of `uid` whose command line carries one of `tokens`.
pub fn procs_with_token<'a>(procs: &'a [Proc], uid: u32, tokens: &[Token]) -> Vec<&'a Proc> {
    procs_of_uid(procs, uid)
        .into_iter()
        .filter(|proc| !find_tokens(&proc.command, tokens).is_empty())
        .collect()
}

/// The lines of `text` (the output of `launchctl print user/<uid>`) that carry a token,
/// trimmed.
pub fn lines_with_token(text: &str, tokens: &[Token]) -> Vec<String> {
    text.lines()
        .filter(|line| !find_tokens(line, tokens).is_empty())
        .map(|line| line.trim().to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const IPCS: &str = "IPC status from <running system> as of Sun Nov  1 12:00:00 PST 2026
Message Queues:
T     ID     KEY        MODE       OWNER    GROUP
q  65536 0x6789abcd --rw-------      700      700
q  65537 0x0000002a --rw-r--r--     root    wheel

Shared Memory:
T     ID     KEY        MODE       OWNER    GROUP
m  131072 0x89abcdef --rw-------  kbf-lease-3-4      700
m  131073 0x00000000 --rw-rw-rw-    alice    staff

Semaphores:
T     ID     KEY        MODE       OWNER    GROUP
s  196608 0x01234567 --ra-------      700      700
s  196609 0x0000beef --ra-ra-ra-     root    wheel
";

    /// Asserts that the three tables of `ipcs -a` are read with their kinds, ids, keys and
    /// owners, and the headings and header rows are not rows. It catches a reader that
    /// takes the `T ID KEY ...` row, or the tables after the first, for nothing.
    #[test]
    fn ipcs_is_read_table_by_table() {
        let objects = parse_ipcs(IPCS);
        assert_eq!(objects.len(), 6);
        assert_eq!(
            objects[0],
            IpcObject {
                kind: IpcKind::Message,
                id: 65536,
                key: "0x6789abcd".to_owned(),
                owner: "700".to_owned(),
            }
        );
        let kinds: Vec<IpcKind> = objects.iter().map(|object| object.kind).collect();
        assert_eq!(
            kinds,
            [
                IpcKind::Message,
                IpcKind::Message,
                IpcKind::Shared,
                IpcKind::Shared,
                IpcKind::Semaphore,
                IpcKind::Semaphore
            ]
        );
        assert_eq!(objects[5].id, 196_609);
        assert!(parse_ipcs("").is_empty());
        assert!(parse_ipcs("Message Queues:\nT ID KEY MODE OWNER GROUP\n").is_empty());
    }

    /// Asserts that the owner matches by number and by the user names given, and not by a
    /// longer number that starts the same. It catches a filter that sees only names: after
    /// a delete `ipcs` prints the bare uid, which is the case this test exists for.
    #[test]
    fn objects_are_owned_by_uid_number_or_name() {
        let objects = parse_ipcs(IPCS);
        let mine = ipc_owned_by(&objects, 700, &["kbf-lease-3-4"]);
        let ids: Vec<u64> = mine.iter().map(|object| object.id).collect();
        assert_eq!(ids, [65536, 131_072, 196_608]);
        let by_number = ipc_owned_by(&objects, 700, &[]);
        assert_eq!(by_number.len(), 2);
        assert!(ipc_owned_by(&objects, 70, &[]).is_empty());
        assert!(ipc_owned_by(&objects, 7000, &[]).is_empty());
    }

    const PS: &str = "    0     1 /sbin/launchd
  700  4242 /bin/sh -c /bin/sleep 600 0123456789abcdef
  700  4243 /bin/sleep 600
  701  4300 /bin/sh -c /bin/sleep 600 0123456789abcdef
  501   777 /Applications/Some App.app/Contents/MacOS/Some App --flag
";

    /// Asserts that `ps` rows keep the whole command line, spaces and all, and that the
    /// matcher by uid and token takes only the processes of that uid that carry a
    /// token. It catches a reader that cuts the command at its first space, and a filter
    /// that forgets the uid (another uid's process would be called ours).
    #[test]
    fn processes_are_read_whole_and_matched_by_uid_and_token() {
        let procs = parse_ps(PS);
        assert_eq!(procs.len(), 5);
        assert_eq!(
            procs[4],
            Proc {
                uid: 501,
                pid: 777,
                command: "/Applications/Some App.app/Contents/MacOS/Some App --flag".to_owned(),
            }
        );
        assert_eq!(procs_of_uid(&procs, 700).len(), 2);
        let token = Token::parse("0123456789abcdef").unwrap();
        let hit = procs_with_token(&procs, 700, std::slice::from_ref(&token));
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].pid, 4242);
        assert!(procs_with_token(&procs, 702, std::slice::from_ref(&token)).is_empty());
        assert!(parse_ps("  UID   PID COMMAND\n").is_empty());
    }

    /// Asserts that the lines of `launchctl print` that carry a token are returned
    /// trimmed and the others are not. It catches a view that finds a label in the
    /// output but reports the whole document.
    #[test]
    fn launchctl_lines_with_a_token_are_returned_trimmed() {
        let token = Token::parse("0123456789abcdef").unwrap();
        let text = "user/700 = {\n\tservices = {\n\t\t0  -  com.komira.iso.0123456789abcdef\n\t\t0  -  com.apple.lsd\n\t}\n}\n";
        assert_eq!(
            lines_with_token(text, std::slice::from_ref(&token)),
            ["0  -  com.komira.iso.0123456789abcdef"]
        );
        assert!(lines_with_token("Could not find domain for user/700\n", &[token]).is_empty());
    }
}
