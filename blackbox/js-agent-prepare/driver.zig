//! The driver of `//blackbox/js-agent-prepare:test`: the released js-agent's update hook, run as hostkeeper runs it, refusing at `prepare` an
//! update whose agent would not start because a mount root the host's flags name is missing (tierstore's mount, which the agent's start checks).
//!
//! Its one argument is the root the js-agent package was put onto (`/` on a farm). In a directory of its own it lays out what a host gives the hook:
//! a staged release (the package's own agent and unit, as hostkeeper stages them), the hook's state directory, the join token, a flags file that names
//! the read-only mount root `trees` at a directory that is not there, and the params of a first install, as `cargo xtask release desired` writes them
//! for a Linux host. Then it runs `<root>/usr/bin/js-agent update-hook prepare <staged> --params <file>` twice:
//!
//! 1. with the mount root missing: passes when the hook exits 75 (deferred) and its report line says `"reason":"host_setup_missing"` and names
//!    `--mount-root-ro trees`, with no record written in the state directory;
//! 2. with the directory made: passes when the hook exits 0 and its report line names no reason.
//!
//! Exit 0 when both passed, 1 when either did not (what the hook printed is on the test's output), 2 for a wrong command line. Catches a release
//! whose `prepare` lets the cutover stop the running agent for one that refuses to start on the host's flags.

const std = @import("std");

fn say(comptime fmt: []const u8, args: anytype) void {
    var buf: [4096]u8 = undefined;
    const line = std.fmt.bufPrint(&buf, "driver: " ++ fmt ++ "\n", args) catch return;
    std.fs.File.stdout().writeAll(line) catch {};
}

/// The unit the package installs for the crun runtime, which the params name and the staged release carries.
const UNIT = "js-agent-crun.service";

/// What one run of the hook ended with.
const Ran = struct {
    code: u8,
    last_line: []const u8,
};

/// Writes `text` to `path`, mode `mode`.
fn put(dir: std.fs.Dir, path: []const u8, text: []const u8, mode: std.fs.File.Mode) !void {
    var file = try dir.createFile(path, .{ .mode = mode, .truncate = true });
    defer file.close();
    try file.writeAll(text);
    try file.chmod(mode);
}

/// The version the agent at `agent` prints: the second word of the first line of `--version`.
fn versionOf(a: std.mem.Allocator, agent: []const u8) ![]const u8 {
    const out = try std.process.Child.run(.{ .allocator = a, .argv = &.{ agent, "--version" } });
    var lines = std.mem.splitScalar(u8, out.stdout, '\n');
    var words = std.mem.tokenizeScalar(u8, lines.first(), ' ');
    _ = words.next() orelse return error.NoVersion;
    return words.next() orelse error.NoVersion;
}

/// Runs the hook's `prepare` of `staged` with the params file `params`, its streams printed on the test's output.
fn prepare(a: std.mem.Allocator, agent: []const u8, staged: []const u8, params: []const u8) !Ran {
    const out = try std.process.Child.run(.{
        .allocator = a,
        .argv = &.{ agent, "update-hook", "prepare", staged, "--params", params },
        .max_output_bytes = 1 << 20,
    });
    std.fs.File.stdout().writeAll(out.stdout) catch {};
    std.fs.File.stderr().writeAll(out.stderr) catch {};
    const code: u8 = switch (out.term) {
        .Exited => |c| c,
        else => 255,
    };
    const trimmed = std.mem.trimRight(u8, out.stdout, "\n");
    const at = if (std.mem.lastIndexOfScalar(u8, trimmed, '\n')) |i| i + 1 else 0;
    return .{ .code = code, .last_line = trimmed[at..] };
}

fn contains(haystack: []const u8, needle: []const u8) bool {
    return std.mem.indexOf(u8, haystack, needle) != null;
}

pub fn drive(root: []const u8) !u8 {
    var arena = std.heap.ArenaAllocator.init(std.heap.page_allocator);
    defer arena.deinit();
    const a = arena.allocator();
    const path = std.fs.path.join;

    const agent = try path(a, &.{ root, "usr/bin/js-agent" });
    const unit = try path(a, &.{ root, "usr/lib/systemd/system", UNIT });
    const version = versionOf(a, agent) catch |e| {
        say("{s} --version: {s}", .{ agent, @errorName(e) });
        return 1;
    };

    // A directory of the run's own, below the root's /var/lib (root's, as the hook holds the directories above its state directory to be).
    const work = try std.fmt.allocPrint(a, "{s}/var/lib/js-agent-blackbox-{d}", .{ std.mem.trimRight(u8, root, "/"), std.os.linux.getpid() });
    try std.fs.cwd().makePath(work);
    var dir = try std.fs.cwd().openDir(work, .{});
    defer dir.close();
    try dir.makePath("staged/bin");
    try dir.makePath("staged/units");
    try dir.makePath("link");
    try dir.makeDir("state");
    var state = try dir.openDir("state", .{});
    try state.chmod(0o700);
    state.close();
    try dir.symLink(agent, "staged/bin/js-agent", .{});
    const unit_text = std.fs.cwd().readFileAlloc(a, unit, 1 << 20) catch |e| {
        say("{s}: {s}", .{ unit, @errorName(e) });
        return 1;
    };
    try put(dir, "staged/units/" ++ UNIT, unit_text, 0o644);
    try put(dir, "token", "tok-blackbox-0123456789\n", 0o600);

    const trees = try path(a, &.{ work, "tierstore-fs" });
    const flags = try std.fmt.allocPrint(a, "--server=https://leases.example:7300\n--pool=farm\n--mount-root-ro=trees={s}\n", .{trees});
    try put(dir, "flags", flags, 0o600);
    const params = try std.fmt.allocPrint(a,
        \\cordon_lease_secs = 300
        \\cordon_max_secs = 17355
        \\drain_wait_secs = 3600
        \\defer_retry_secs = 300
        \\release_link = "{0s}/link/current"
        \\unit = "{1s}"
        \\state_dir = "{0s}/state"
        \\flags_file = "{0s}/flags"
        \\state_root = "{0s}/no-state-root-yet"
        \\token_file = "{0s}/token"
        \\runtime = "crun"
        \\prepare_timeout_secs = 3900
        \\cutover_timeout_secs = 4320
        \\verify_timeout_secs = 615
        \\rollback_timeout_secs = 8220
        \\cleanup_timeout_secs = 120
        \\verify_deadline_secs = 9000
        \\"updater.artifact_digest" = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        \\"updater.staged_release" = "{0s}/staged"
        \\"updater.target_version" = "{2s}"
        \\
    , .{ work, UNIT, version });
    try put(dir, "params.toml", params, 0o600);
    const staged = try path(a, &.{ work, "staged" });
    const params_file = try path(a, &.{ work, "params.toml" });

    say("js-agent {s}: prepare of a first install with the mount root {s} missing", .{ version, trees });
    const missing = try prepare(a, agent, staged, params_file);
    const recorded = if (dir.access("state/record.json", .{})) |_| true else |_| false;
    const refused = missing.code == 75 and contains(missing.last_line, "\"reason\":\"host_setup_missing\"") and
        contains(missing.last_line, "--mount-root-ro trees") and !recorded;
    if (!refused) {
        say("FAIL: wanted exit 75, reason host_setup_missing naming --mount-root-ro trees, and no record; got exit {d}, record {}, line {s}", .{ missing.code, recorded, missing.last_line });
        return 1;
    }
    say("pass: the update was deferred at prepare, naming the mount root: {s}", .{missing.last_line});

    try std.fs.cwd().makePath(trees);
    say("prepare again with the mount root there", .{});
    const present = try prepare(a, agent, staged, params_file);
    if (present.code != 0 or contains(present.last_line, "\"reason\"")) {
        say("FAIL: wanted exit 0 with no reason; got exit {d}, line {s}", .{ present.code, present.last_line });
        return 1;
    }
    say("pass: prepare went on with the mount root there: {s}", .{present.last_line});
    return 0;
}

pub fn main() u8 {
    var arena = std.heap.ArenaAllocator.init(std.heap.page_allocator);
    defer arena.deinit();
    const argv = std.process.argsAlloc(arena.allocator()) catch return 1;
    if (argv.len != 2) {
        say("the one argument is the root the js-agent package was put onto", .{});
        return 2;
    }
    return drive(argv[1]) catch |e| {
        say("FAIL: {s}", .{@errorName(e)});
        return 1;
    };
}
