use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::{
    Claim, Claims, Clock, Covers, Ctime, ENDED, Held, Journal, Kind, Namespace, Object, Runner,
    STARTED, Standing, Swept, abandoned, alive, claimed, left, machine, offset, second_of_day,
    unnamed_left, window,
};
use crate::oracle::{Process, Ran};
use crate::scratch::Tree;

/// `ipcs -a` as macOS printed it on 2026-10-01, cut to one object or two
/// of each kind: a database's segment and semaphores, and what the
/// subsets' tests left.
const IPCS: &str = "\
IPC status from <running system> as of Thu Oct  1 23:49:12 CEST 2026
T     ID     KEY        MODE       OWNER    GROUP  CREATOR   CGROUP CBYTES  QNUM QBYTES LSPID LRPID   STIME    RTIME    CTIME
Message Queues:
q 7536640 0x00000000 ----------- mitchell    staff mitchell    staff      0      0   2048      0      0 no-entry no-entry 23:42:09
q 1441795 0x00000001 --rw-rw-rw- mitchell    staff mitchell    staff      0      0   2048   4242   4243 23:48:10 23:48:10 23:48:10

T     ID     KEY        MODE       OWNER    GROUP  CREATOR   CGROUP NATTCH  SEGSZ  CPID  LPID   ATIME    DTIME    CTIME
Shared Memory:
m  65536 0x042e2346 --rw------- mitchell    staff mitchell    staff      6     56    824    824 20:45:10 21:56:32 20:45:10
m 6750211 0x00000000 --rw-rw-rw- mitchell    staff mitchell    staff      0   1024  77085  77090 23:48:08 23:48:08 23:48:08

T     ID     KEY        MODE       OWNER    GROUP  CREATOR   CGROUP NSEMS   OTIME    CTIME
Semaphores:
s  65539 0x042e2347 --ra------- mitchell    staff mitchell    staff     20 20:45:10 20:45:10
s 131082 0x00000000 ----------- mitchell    staff mitchell    staff      3 no-entry  1:46:23
";

fn object(kind: Kind, id: u64, named: &[u32], attached: u64) -> Object {
    Object {
        kind,
        id,
        named: named.iter().copied().collect(),
        attached,
        key: 0,
        creator: "mitchell".to_owned(),
        ctime: None,
    }
}

/// An object as `ipcs` prints one: its key, its creator and its `CTIME`.
fn made(kind: Kind, id: u64, named: &[u32], key: u64, creator: &str, ctime: &str) -> Object {
    Object {
        key,
        creator: creator.to_owned(),
        ctime: second_of_day(ctime).map(Ctime::Day),
        ..object(kind, id, named, 0)
    }
}

#[test]
fn ipcs_is_read_by_each_sections_header() {
    let standing = Standing::parse(IPCS);
    let m = |id, named, key, ctime| made(Kind::Memory, id, named, key, "mitchell", ctime);
    assert_eq!(
        standing.0,
        [
            made(Kind::Queue, 7_536_640, &[], 0, "mitchell", "23:42:09"),
            made(
                Kind::Queue,
                1_441_795,
                &[4242, 4243],
                1,
                "mitchell",
                "23:48:10"
            ),
            Object {
                attached: 6,
                ..m(65_536, &[824], 0x042e_2346, "20:45:10")
            },
            m(6_750_211, &[77_085, 77_090], 0, "23:48:08"),
            made(
                Kind::Semaphores,
                65_539,
                &[],
                0x042e_2347,
                "mitchell",
                "20:45:10"
            ),
            made(Kind::Semaphores, 131_082, &[], 0, "mitchell", "1:46:23"),
        ]
    );
}

/// `/proc/sysvipc/msg`, `shm` and `sem` as Linux prints them, in the
/// formats of `ipc/msg.c`, `ipc/shm.c` and `ipc/sem.c`: six objects like
/// [`IPCS`]'s, a database's keyed segment and set beside what the subsets'
/// tests left, one key above `INT_MAX` printed negative.
const PROC_MSG: &str = "\
       key      msqid perms      cbytes       qnum lspid lrpid   uid   gid  cuid  cgid      stime      rtime      ctime
         0          0     0           0          0     0     0  1000  1000  1000  1000          0          0 1759355129
         1      32769   666           0          0  4242  4243  1000  1000  1000  1000 1759355290 1759355290 1759355290
";
const PROC_SHM: &str = "\
       key      shmid perms                  size  cpid  lpid nattch   uid   gid  cuid  cgid      atime      dtime      ctime                   rss                  swap
  70132550          0   600                    56   824   824      6  1000  1000  1000  1000 1759344310 1759348592 1759344310                  4096                     0
         0      32769   666                  1024 77085 77090      0  1000  1000  1000  1000 1759355288 1759355288 1759355288                     0                     0
";
const PROC_SEM: &str = "\
       key      semid perms      nsems   uid   gid  cuid  cgid      otime      ctime
-889275714          0   600         20  1000  1000   999   999 1759344310 1759344310
         0      32769     0          3  1000  1000  1000  1000          0 1759282783
";

/// Linux's `/proc/sysvipc` read as `ipcs -a` is on macOS (#1643): each
/// object with its id, the processes the kernel names on it, its attached
/// count, its key as the `key_t` it is, its creator's uid and its `ctime`
/// as the instant it prints. A header alone, as a machine with no object
/// prints, is no object.
#[test]
fn proc_sysvipc_is_read_by_each_files_header() {
    let standing = Standing::of_proc(PROC_SHM, PROC_MSG, PROC_SEM);
    let at = |kind, id, named: &[u32], key, creator: &str, ctime| Object {
        key,
        creator: creator.to_owned(),
        ctime: Some(Ctime::Epoch(ctime)),
        ..object(kind, id, named, 0)
    };
    assert_eq!(
        standing.0,
        [
            at(Kind::Queue, 0, &[], 0, "1000", 1_759_355_129),
            at(Kind::Queue, 32_769, &[4242, 4243], 1, "1000", 1_759_355_290),
            Object {
                attached: 6,
                ..at(Kind::Memory, 0, &[824], 0x042e_2346, "1000", 1_759_344_310)
            },
            at(
                Kind::Memory,
                32_769,
                &[77_085, 77_090],
                0,
                "1000",
                1_759_355_288
            ),
            at(Kind::Semaphores, 0, &[], 0xcafe_babe, "999", 1_759_344_310),
            at(Kind::Semaphores, 32_769, &[], 0, "1000", 1_759_282_783),
        ]
    );
    let header = |listed: &str| listed.lines().next().unwrap().to_owned() + "\n";
    assert_eq!(
        Standing::of_proc(&header(PROC_SHM), &header(PROC_MSG), &header(PROC_SEM)),
        Standing::default()
    );
}

/// The rule for an object naming no process holds over Linux's `ctime`,
/// an instant and not a time of day, as it does over `ipcs`'s: a private
/// queue of the runner's uid made while a program ran is removed, and one
/// made a second before it began, or another uid's, stands (#1643).
#[test]
fn an_instant_from_proc_is_held_to_the_programs_runs_as_a_time_of_day_is() {
    let q = |id, creator: &str, after: i64| Object {
        creator: creator.to_owned(),
        ctime: Some(Ctime::Epoch(AT_2300 + after)),
        ..object(Kind::Queue, id, &[], 0)
    };
    let after = Standing(vec![q(1, "501", 5), q(2, "501", 3), q(3, "1000", 5)]);
    let gone: Vec<u64> = unnamed_left(
        &Standing::default(),
        &after,
        &[ran(4_500, 6_200)],
        (&runner(), &clock()),
    )
    .into_iter()
    .map(|o| o.id)
    .collect();
    assert_eq!(gone, [1]);
}

/// The rule, one counterexample per clause: only an object new since the
/// subset began, with nothing attached, naming a process of the run and
/// no living process, is the run's.
#[test]
fn only_what_the_runs_own_processes_left_is_removed() {
    let before = Standing(vec![
        // A database's segment, standing before the run, whose creator's
        // id the run's process was later handed again.
        object(Kind::Memory, 1, &[100], 0),
    ]);
    let after = Standing(vec![
        object(Kind::Memory, 1, &[100], 0),
        // A segment a test created and left.
        object(Kind::Memory, 2, &[100], 0),
        // One another process created during the run.
        object(Kind::Memory, 3, &[900], 0),
        // One a test created and something still holds.
        object(Kind::Memory, 4, &[101], 1),
        // A queue a test sent to and another program, still alive, read.
        object(Kind::Queue, 5, &[101, 500], 0),
        // A queue a test sent to and nobody read.
        object(Kind::Queue, 6, &[101], 0),
        // A semaphore set a test operated on.
        object(Kind::Semaphores, 7, &[102], 0),
        // A queue nobody sent to: nothing says whose it is.
        object(Kind::Queue, 8, &[], 0),
    ]);
    let spawned: BTreeSet<u32> = [100, 101, 102].into();
    let alive = |pid| pid == 500;
    let gone: Vec<(Kind, u64)> = left(&before, &after, &spawned, alive)
        .into_iter()
        .map(|o| (o.kind, o.id))
        .collect();
    assert_eq!(
        gone,
        [(Kind::Memory, 2), (Kind::Queue, 6), (Kind::Semaphores, 7)]
    );
}

#[test]
fn the_sweep_says_how_many_it_removed_of_each_kind_and_what_it_kept() {
    let swept = Swept {
        removed: BTreeMap::from([(Kind::Memory, vec![7, 9]), (Kind::Queue, vec![8])]),
        unnamed_removed: 1,
        unnamed: 1,
    };
    assert_eq!(
        swept.to_string(),
        "removed 3 System V objects (shared memory segments 7 9, message queue 8), 1 of them naming no process, the runner's, private and made while one of the run's programs ran; 1 new one names no process and stands, since nothing tells the run's from another program's"
    );
    assert_eq!(Swept::default().to_string(), "removed 0 System V objects");
}

/// 2026-10-01 23:00:00 in a zone two hours east of UTC, as seconds since
/// the epoch, and the clock a sweep read then.
const AT_2300: i64 = 1_759_352_400;

fn clock() -> Clock {
    Clock {
        now: AT_2300 + 3600,
        east: 7200,
    }
}

/// A program's run from `from` milliseconds past 23:00 local to `to`.
fn ran(from: u64, to: u64) -> Ran {
    let at = |ms: u64| {
        UNIX_EPOCH
            + Duration::from_secs(u64::try_from(AT_2300).unwrap())
            + Duration::from_millis(ms)
    };
    Ran::between(at(from), Some(at(to)))
}

fn runner() -> Runner {
    Runner {
        name: "mitchell".to_owned(),
        uid: 501,
    }
}

/// The rule Mitchell ruled on #1469 (#1482), one counterexample per
/// clause: an object naming no process is the run's only when it is new
/// since the subset began, the runner's, made under `IPC_PRIVATE`, and
/// made in a second one of the subset's programs ran in, each program's
/// run rounded out to whole seconds. Without the rule the first two
/// stand, as the process-named sweep leaves them.
#[test]
fn a_private_object_of_the_runners_made_while_a_program_ran_is_removed_and_nothing_else() {
    let before = Standing(vec![
        // Made under the same key by the runner a day before, at the same
        // second of the day: it stood before the subset began.
        made(Kind::Queue, 1, &[], 0, "mitchell", "23:00:05"),
    ]);
    let q = |id, key, creator, ctime| made(Kind::Queue, id, &[], key, creator, ctime);
    let after = Standing(vec![
        q(1, 0, "mitchell", "23:00:05"),
        // php's msg_get_queue(0) in a test, mode 0, nobody's sender.
        q(2, 0, "mitchell", "23:00:05"),
        // The same by number, where the user database names nobody.
        q(3, 0, "501", "23:00:12"),
        // A semaphore set a test made and nobody operated on.
        made(Kind::Semaphores, 4, &[], 0, "mitchell", "23:00:06"),
        // Made under IPC_PRIVATE by a process outside the subset before
        // its first program started.
        q(5, 0, "mitchell", "23:00:02"),
        // A keyed queue the runner's own user made while a test ran.
        q(6, 0x5a00_0001, "mitchell", "23:00:05"),
        // Another user's private queue of the same second.
        q(7, 0, "postgres", "23:00:05"),
        // Made between two programs, a whole second from either.
        q(8, 0, "mitchell", "23:00:09"),
        // A segment something still holds.
        Object {
            attached: 1,
            ..made(Kind::Memory, 9, &[], 0, "mitchell", "23:00:05")
        },
        // A queue that names a process is the other rule's.
        made(Kind::Queue, 10, &[77], 0, "mitchell", "23:00:05"),
    ]);
    // Two programs, from 23:00:04.5 to 23:00:06.2 and from 23:00:11 to
    // 23:00:11.5, and one not seen to end, from 23:00:20.
    let mut runs = vec![ran(4_500, 6_200), ran(11_000, 11_500)];
    runs.push(Ran::between(
        UNIX_EPOCH + Duration::from_secs(u64::try_from(AT_2300).unwrap() + 20),
        None,
    ));
    let gone: Vec<(Kind, u64)> = unnamed_left(&before, &after, &runs, (&runner(), &clock()))
        .into_iter()
        .map(|o| (o.kind, o.id))
        .collect();
    assert_eq!(
        gone,
        [(Kind::Queue, 2), (Kind::Queue, 3), (Kind::Semaphores, 4)]
    );
    // The control: without a program's run there is no second to hold it
    // to, and nothing that names no process is taken.
    assert!(unnamed_left(&before, &after, &[], (&runner(), &clock())).is_empty());
}

/// `ipcs` prints the local time of day, which is the latest such second
/// not after the sweep: before midnight on the day before when the clock
/// has passed it.
#[test]
fn a_time_of_day_is_the_latest_such_second_not_after_now() {
    let clock = clock();
    assert_eq!(clock.when(23 * 3600), AT_2300);
    // 00:30 local is past the clock's 00:00 and so the day before's.
    let past_midnight = Clock {
        now: AT_2300 + 3600,
        east: 7200,
    };
    assert_eq!(past_midnight.when(1800), AT_2300 + 1800 - 86_400 + 3600);
    assert_eq!(offset("+0200"), Some(7200));
    assert_eq!(offset("-0530"), Some(-19_800));
    assert_eq!(offset("CEST"), None);
    assert_eq!(second_of_day("1:46:23"), Some(6383));
    assert_eq!(second_of_day("no-entry"), None);
    let now = SystemTime::now();
    assert!(Clock::now().is_ok_and(
        |c| c.now >= i64::try_from(now.duration_since(UNIX_EPOCH).unwrap().as_secs()).unwrap()
    ));
}

/// The two namespaces tried on Linux, the bare one first and then one in
/// the runner's own user namespace, and what the run's line says of one.
#[test]
fn the_namespace_is_unshare_around_the_binary() {
    let [bare, user] = Namespace::tries("/usr/bin/unshare");
    assert_eq!(bare.0, ["/usr/bin/unshare", "--ipc", "--"]);
    assert_eq!(
        user.0,
        [
            "/usr/bin/unshare",
            "--user",
            "--map-current-user",
            "--ipc",
            "--"
        ]
    );
    assert_eq!(
        bare.to_string(),
        "each program runs in a private IPC namespace (unshare --ipc)"
    );
    assert_eq!(Namespace::of("ext/standard"), None);
    if cfg!(not(target_os = "linux")) {
        assert_eq!(Namespace::of("ext/sysvmsg"), None);
    }
}

/// Which namespace a System V subset runs in on Linux: the bare one where
/// `unshare --ipc` runs, as root or in a container with the capability
/// does; the one in the runner's own user namespace where only that runs,
/// as an unprivileged user does where the kernel allows it; and none
/// where neither does, as a container under the default seccomp profile
/// refuses both, where the sweep is what removes a test's objects (#1582).
#[test]
fn the_first_namespace_unshare_permits_is_the_one_a_subset_runs_in() {
    let [bare, user] = Namespace::tries("/usr/bin/unshare");
    assert_eq!(
        Namespace::chosen("/usr/bin/unshare", |_| true),
        Some(bare.clone())
    );
    assert_eq!(
        Namespace::chosen("/usr/bin/unshare", |ns| *ns == user),
        Some(user.clone())
    );
    assert_eq!(
        user.to_string(),
        "each program runs in a private IPC namespace (unshare --user --map-current-user --ipc)"
    );
    assert_eq!(Namespace::chosen("/usr/bin/unshare", |_| false), None);
}

/// On Linux, where `unshare` permits one, an object a program makes in the
/// namespace a System V subset runs in goes with the program: a queue
/// `ipcmk` makes under it is not on the machine after (#1582). Where it
/// permits neither, the test says so and returns.
#[cfg(target_os = "linux")]
#[test]
fn a_queue_made_in_the_namespace_does_not_outlive_its_program() {
    let Some(namespace) = Namespace::of("ext/sysvmsg") else {
        eprintln!("skipped: unshare permits neither namespace here");
        return;
    };
    let keys = |listed: &[u8]| -> Vec<String> {
        String::from_utf8_lossy(listed)
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .filter(|key| key.starts_with("0x"))
            .map(str::to_owned)
            .collect()
    };
    let (program, args) = namespace.0.split_first().unwrap();
    let inside = std::process::Command::new(program)
        .args(args)
        .args(["sh", "-c", "ipcmk -Q >/dev/null && ipcs -q"])
        .output()
        .unwrap();
    assert!(inside.status.success(), "{inside:?}");
    let made = keys(&inside.stdout);
    assert_eq!(made.len(), 1, "one queue in the namespace: {inside:?}");
    let outside = std::process::Command::new("ipcs")
        .arg("-q")
        .output()
        .unwrap();
    assert!(
        !keys(&outside.stdout).contains(&made[0]),
        "the queue {} outlived its namespace: {outside:?}",
        made[0]
    );
}

/// A witness run's claim, one counterexample per clause (#2115): an
/// object is the run's when it is private, the runner's, attached to
/// nothing, names no living process, and was made inside the claim's
/// window, whether it names a process or none.
#[test]
fn a_claim_takes_what_is_forsaken_inside_its_window_and_nothing_else() {
    let standing = Standing(vec![
        // A segment the served process made and left, its creator gone.
        made(Kind::Memory, 1, &[100], 0, "mitchell", "23:00:05"),
        // php's msg_get_queue(0)'s probe, mode 0, naming nobody.
        made(Kind::Queue, 2, &[], 0, "mitchell", "23:00:06"),
        // The same a second before the run began.
        made(Kind::Queue, 3, &[], 0, "mitchell", "23:00:03"),
        // One a living process still names.
        made(Kind::Memory, 4, &[500], 0, "mitchell", "23:00:05"),
        // One something still holds.
        Object {
            attached: 1,
            ..made(Kind::Memory, 5, &[100], 0, "mitchell", "23:00:05")
        },
        // A keyed one, and another user's.
        made(
            Kind::Semaphores,
            6,
            &[],
            0x5a00_0001,
            "mitchell",
            "23:00:05",
        ),
        made(Kind::Queue, 7, &[], 0, "postgres", "23:00:05"),
    ]);
    let alive = |pid| pid == 500;
    let window = [(AT_2300 + 4, AT_2300 + 10)];
    let gone: Vec<u64> = claimed(&standing, &window, (&runner(), &clock()), alive)
        .into_iter()
        .map(|o| o.id)
        .collect();
    assert_eq!(gone, [1, 2]);
    // The control: no window, nothing taken.
    assert!(claimed(&standing, &[], (&runner(), &clock()), alive).is_empty());
}

/// A private segment of the runner's that nothing attaches and whose
/// every named process is dead is taken whatever run made it, as the
/// segments that filled the table on 2026-10-06 were by hand (#2115); one
/// a living process last attached, a queue, and a segment naming nobody
/// are not.
#[test]
fn a_dead_creators_private_segment_is_abandoned_whatever_run_made_it() {
    let standing = Standing(vec![
        made(Kind::Memory, 1, &[100], 0, "mitchell", "6:28:37"),
        made(Kind::Memory, 2, &[100, 500], 0, "mitchell", "6:28:37"),
        made(Kind::Memory, 3, &[], 0, "mitchell", "6:28:37"),
        made(Kind::Queue, 4, &[100], 0, "mitchell", "6:28:37"),
        made(Kind::Memory, 5, &[100], 0x042e_2346, "mitchell", "6:28:37"),
    ]);
    let gone: Vec<u64> = abandoned(&standing, &runner(), |pid| pid == 500)
        .into_iter()
        .map(|o| o.id)
        .collect();
    assert_eq!(gone, [1]);
}

/// The claims of `dir` once no other process holds the ones a test let
/// go: a process forked in the moment a claim was let go holds the
/// descriptors of the one it came from until it execs, its lock with them.
fn ended(dir: &Tree, claims: usize) -> Claims {
    for _ in 0..20 {
        let read = Claims::read(dir, None);
        if read.held == Held::default() && read.ended.len() == claims {
            return read;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("the claims of {} are still held", dir.display());
}

/// The ids of what `perl` prints, made by `script`, a space between each.
fn perl(script: &str) -> Vec<u64> {
    let made = std::process::Command::new("perl")
        .args(["-MIPC::SysV=IPC_PRIVATE,IPC_CREAT", "-e", script])
        .output()
        .unwrap();
    assert!(made.status.success(), "{made:?}");
    let printed = String::from_utf8(made.stdout).unwrap();
    printed.split(' ').map(|id| id.parse().unwrap()).collect()
}

/// `ids`' queues among `taken`, in order.
fn queues(taken: &[&Object], ids: &[u64]) -> Vec<u64> {
    let mut queues: Vec<u64> = taken
        .iter()
        .filter(|o| o.kind == Kind::Queue && ids.contains(&o.id))
        .map(|o| o.id)
        .collect();
    queues.sort_unstable();
    queues
}

/// Two runs at once (#2563), each with a claim in one directory, and a
/// private queue of mode 0 nobody sent to standing for each, as php's
/// `msg_get_queue(0)` leaves one, made in one second: the kernel names no
/// maker on either, so one run's and the other's look alike. The first
/// run's window took both before; while the second run holds its claim
/// it takes neither. Once both runs have ended unsettled, as runs killed
/// whole leave their claims, a reclaim takes both and the claims with
/// them. Only what is decided is read: the test removes its own two
/// queues and nothing else on the machine.
#[test]
fn what_is_made_while_another_run_holds_its_claim_is_left_until_both_end() {
    let dir = Tree::new("sysv-held").unwrap();
    let ours = Claim::laid(&dir, "ours", Covers::Window).unwrap();
    let theirs = Claim::laid(&dir, "theirs", Covers::Window).unwrap();
    let mut ids =
        perl("print msgget(IPC_PRIVATE, 0) // die $!, ' ', msgget(IPC_PRIVATE, 0) // die $!");
    ids.sort_unstable();
    let (standing, runner, clock) = (
        Standing::now().unwrap(),
        Runner::now(),
        Clock::now().unwrap(),
    );
    let windows = [window(ours.from, None, &clock)];
    let mut taken = claimed(&standing, &windows, (&runner, &clock), alive);
    assert_eq!(queues(&taken, &ids), ids, "the rule before #2563");
    let held = Claims::read(&dir, Some(ours.path())).held;
    taken.retain(|o| !held.covers(o, &clock));
    let refused = queues(&taken, &ids);
    let paths = [ours.path().to_path_buf(), theirs.path().to_path_buf()];
    drop((ours, theirs));
    let grace = Duration::from_secs(1);
    let claims = ended(&dir, 2);
    let reclaimed = queues(
        &claims.taken(&standing, grace, (&runner, &clock), alive),
        &ids,
    );
    for id in &ids {
        std::process::Command::new("ipcrm")
            .args(["-q", &id.to_string()])
            .status()
            .ok();
    }
    assert_eq!(refused, [0u64; 0], "taken while another run held its claim");
    assert_eq!(reclaimed, ids, "both reclaimed once both runs ended");
    claims.settle(grace, &clock);
    assert!(paths.iter().all(|path| !path.exists()), "the claims go");
}

/// A claim of a run that ended unsettled whose window reaches a second
/// another run's claim covers stands for a later reclaim, and goes once
/// no other run holds one.
#[test]
fn an_ended_claim_another_run_held_beside_stands_until_it_ends() {
    let dir = Tree::new("sysv-beside").unwrap();
    let theirs = Claim::laid(&dir, "theirs", Covers::Window).unwrap();
    let path = Claim::laid(&dir, "ours", Covers::Window)
        .unwrap()
        .path()
        .to_path_buf();
    let clock = Clock::now().unwrap();
    Claims::read(&dir, None).settle(Duration::from_secs(1), &clock);
    assert!(path.exists(), "taken while another run held its claim");
    drop(theirs);
    ended(&dir, 2).settle(Duration::from_secs(1), &clock);
    assert!(!path.exists(), "the claim goes once no other run holds one");
}

/// A claim's journal names the processes of its [`STARTED`] lines alone,
/// and those whose [`ENDED`] line never came apart: a witness run's JSON
/// line, a torn last line, and an end of a number never started name none.
#[test]
fn a_claims_journal_names_the_processes_it_started_and_those_never_seen_to_end() {
    let journal = Journal::read(&format!(
        "{STARTED} 0 4242\n{ENDED} 0\n{{\"entry\":\"serving\"}}\n{STARTED} 1 4243\n{ENDED} 7\n{STARTED} 2"
    ));
    assert_eq!(journal.pids().collect::<Vec<u32>>(), [4242, 4243]);
    assert_eq!(journal.unended().collect::<Vec<_>>(), [(1, 4243)]);
}

/// `ps -o pgid=,etime=` read as the process's group and the second it
/// started, its age in each of the shapes `ps` prints, and nothing read
/// where `ps` printed no process.
#[test]
fn a_process_is_read_by_its_group_and_its_age() {
    let at = |printed| Process::read(printed, 1_000_000).map(|p| (p.group, p.started));
    assert_eq!(at("  4242      00:07\n"), Some((4242, 999_993)));
    assert_eq!(at("4242 01:02:03"), Some((4242, 1_000_000 - 3723)));
    assert_eq!(
        at("1 05-01:47:32"),
        Some((1, 1_000_000 - 5 * 86_400 - 6452))
    );
    assert_eq!(at(""), None);
    assert_eq!(at("4242 no-time"), None);
}

/// A cut-off run's program is the process under its number that leads a
/// group of that number and started by its journal's last write, a
/// second's give (#2522); every counterexample is a process that is not
/// the program and is never signalled: one that took the number after the
/// last write, and one in another process's group, as a reused number
/// most often is.
#[test]
fn a_process_is_a_cut_off_runs_program_only_by_its_number_its_group_and_its_start() {
    let last = 1_060;
    let process = |group, started| Process { group, started };
    assert!(process(4242, 1_000).spawned_as(4242, last));
    assert!(process(4242, 1_061).spawned_as(4242, last));
    assert!(!process(4242, 1_062).spawned_as(4242, last), "reused since");
    assert!(
        !process(77, 1_030).spawned_as(4242, last),
        "another's group"
    );
}

/// A claim a System V subset's run never settled, its journal naming three
/// programs, kills the one that still stands and never ended, with its
/// group, and writes its end (#2522); a process standing under a number
/// whose program ended, as another run's may, and one standing under a
/// started number that leads no group of its own, as a process that took
/// the number would, are never signalled, nor is a program still going
/// under a claim another run holds. Every process is a `sleep` this test
/// started and ends.
#[test]
fn a_claim_left_unsettled_kills_its_programs_still_standing_and_nothing_else() {
    use std::os::unix::process::CommandExt;
    use std::process::Command;
    let sleep = |leads: bool| {
        let mut command = Command::new("sleep");
        command.arg("60");
        if leads {
            command.process_group(0);
        }
        command.spawn().unwrap()
    };
    let dir = Tree::new("sysv-standing").unwrap();
    let claim = Claim::laid(&dir, "sysv-test", Covers::Window).unwrap();
    let path = claim.path().to_path_buf();
    let theirs = Claim::laid(&dir, "theirs", Covers::Window).unwrap();
    let mut program = sleep(true);
    let mut others = [sleep(true), sleep(false), sleep(true)];
    let pids = [program.id(), others[0].id(), others[1].id()];
    std::fs::write(theirs.path(), format!("{STARTED} 0 {}\n", others[2].id())).unwrap();
    std::fs::write(
        &path,
        format!(
            "{STARTED} 0 {}\n{STARTED} 1 {}\n{ENDED} 1\n{STARTED} 2 {}\n",
            pids[0], pids[1], pids[2]
        ),
    )
    .unwrap();
    drop(claim);
    let mut claims = (0..20)
        .map(|_| {
            std::thread::sleep(Duration::from_millis(50));
            Claims::read(&dir, None)
        })
        .find(|read| read.ended.len() == 1)
        .expect("the claim let go is read as ended");
    // The test is the programs' parent, as the killed run was, so it reaps
    // the one killed, as the process orphans pass to would.
    let reaper = std::thread::spawn(move || program.wait().unwrap());
    claims.stop();
    let still: Vec<bool> = others
        .iter_mut()
        .map(|child| child.try_wait().unwrap().is_none())
        .collect();
    let journal = std::fs::read_to_string(&path).unwrap();
    for child in &mut others {
        child.kill().ok();
        child.wait().ok();
    }
    let killed = reaper.join().unwrap();
    assert!(
        !killed.success(),
        "the standing program was killed: {killed:?}"
    );
    assert_eq!(still, [true; 3], "no other process was signalled");
    drop(theirs);
    assert!(journal.ends_with(&format!("{ENDED} 0\n")), "{journal}");
}

/// A claim a phpt subset's run never settled, its journal naming the
/// programs it started, is taken with the objects those programs left
/// whatever their key, as the run's own sweep would have taken them
/// (#2448): a keyed queue a gone process sent to and its journal names is
/// removed, while the same queue of a process no journal names stands.
#[test]
fn a_claim_left_unsettled_takes_what_its_journaled_programs_left() {
    let dir = Tree::new("sysv-journaled").unwrap();
    let claim = Claim::laid(&dir, "sysv-test", Covers::Window).unwrap();
    let path = claim.path().to_path_buf();
    let queue = || {
        perl(
            "my $id = msgget(0x5d000000 + $$, IPC_CREAT | 0600) // die $!; msgsnd($id, pack('l! a*', 1, 'x'), 0) or die $!; print \"$$ $id\"",
        )
    };
    let [pid, journaled] = queue()[..] else {
        panic!()
    };
    let [_, unjournaled] = queue()[..] else {
        panic!()
    };
    std::fs::write(&path, format!("{STARTED} 0 {pid}\nended 0\n")).unwrap();
    drop(claim);
    let (standing, runner, clock) = (
        Standing::now().unwrap(),
        Runner::now(),
        Clock::now().unwrap(),
    );
    let ids = [journaled, unjournaled];
    let taken = queues(
        &ended(&dir, 1).taken(&standing, Duration::from_secs(1), (&runner, &clock), alive),
        &ids,
    );
    for id in ids {
        std::process::Command::new("ipcrm")
            .args(["-q", &id.to_string()])
            .status()
            .ok();
    }
    assert_eq!(taken, [journaled], "the journaled queue alone");
}

/// A held claim's journal keeps every other run's sweep from what its
/// processes are named on (#2750): a phpt subset's test made a segment
/// and ended, and another run's reclaim took it as [`abandoned`] before
/// the subset's own sweep, which counted two of the three objects. The
/// subset's claim of its processes alone holds no window, so a segment
/// another process left is still taken; let go, it goes, and its
/// process's segment is any run's to take again.
#[test]
fn what_a_held_claims_processes_are_named_on_is_no_other_runs_to_take() {
    use std::io::Write as _;

    let dir = Tree::new("sysv-own").unwrap();
    let own = Claim::laid(&dir, "phpt", Covers::Processes).unwrap();
    writeln!(own.journal().unwrap(), "{STARTED} 0 4242").unwrap();
    let standing = Standing(vec![
        made(Kind::Memory, 1, &[4242], 0, "mitchell", "23:30:00"),
        made(Kind::Memory, 2, &[4243], 0, "mitchell", "23:30:00"),
    ]);
    let grace = Duration::from_secs(1);
    let taken = |claims: &Claims| -> Vec<u64> {
        let mut ids: Vec<u64> = claims
            .taken(&standing, grace, (&runner(), &clock()), |_| false)
            .iter()
            .map(|o| o.id)
            .collect();
        ids.sort_unstable();
        ids
    };
    let held = Claims::read(&dir, None);
    assert_eq!(
        held.held.since, None,
        "a claim of its processes alone has no window"
    );
    assert_eq!(
        taken(&held),
        [2],
        "taken while the run that made it held its claim"
    );
    let path = own.path().to_path_buf();
    drop((held, own));
    assert_eq!(taken(&ended(&dir, 0)), [1, 2]);
    assert!(
        !path.exists(),
        "a claim of its processes alone goes once let go"
    );
}

/// A run whose claims a test lays apart reads the claims held in the
/// machine's directory too (#2902): a phpt subset's claim of its processes
/// alone, held there, keeps a run laid apart from the segment its test's
/// process left, which such a run's reclaim took as [`abandoned`] between
/// the test ending and the subset's own sweep. Only this test's claim and
/// pid are read; other runs' claims on the machine may stand beside it.
#[test]
fn a_run_laid_apart_holds_back_from_what_the_machines_held_claims_name() {
    use std::io::Write as _;

    let apart = Tree::new("sysv-apart").unwrap();
    let own = Claim::own("phpt").unwrap();
    assert_eq!(own.path().parent(), Some(machine().as_path()));
    writeln!(own.journal().unwrap(), "{STARTED} 0 4294967295").unwrap();
    assert!(!Claims::read(&apart, None).held.started.contains(&u32::MAX));
    assert!(Claims::seen(&apart, None).held.started.contains(&u32::MAX));
    std::fs::remove_file(own.path()).ok();
}
