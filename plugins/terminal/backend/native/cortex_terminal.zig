//! Pin-specific adapter. Copied into Ghostty's src/ by native/build.py.
//! All inspection is read-only; buffer switching and DECRC happen on decoded clones.
const std = @import("std");
const c = @import("terminal/c/terminal.zig");
const snapshot = @import("terminal/c/snapshot.zig");
const fmt = @import("terminal/formatter.zig");
const modes = @import("terminal/modes.zig");
const Terminal = @import("terminal/Terminal.zig");
const Key = @import("terminal/ScreenSet.zig").Key;
const max_bytes = 16 * 1024 * 1024;

pub fn read(handle: c.Terminal, kind: u8, ptr: [*]u8, capacity: usize, written: *usize) callconv(.c) c_int {
    written.* = 0;
    const t = c.zigTerminal(handle) orelse return -1;
    var processing_error = false;
    if (c.get(handle, .vt_processing_error, &processing_error) != .success or processing_error) return -3;
    var writer: std.Io.Writer = .fixed(ptr[0..capacity]);
    if (kind == 0) {
        var text: fmt.TerminalFormatter = .init(t, .plain);
        text.extra = .none;
        text.format(&writer) catch return -2;
    } else if (kind == 1) {
        restore(handle, &writer) catch return -2;
    } else return -1;
    written.* = writer.end;
    return 0;
}

fn decode(bytes: []const u8) !c.Terminal {
    var decoder: snapshot.Decoder = null;
    if (snapshot.decoder_new_buf(null, &decoder, bytes.ptr, bytes.len) != .success) return error.Decode;
    defer snapshot.decoder_free(decoder);
    var clone: c.Terminal = null;
    if (snapshot.decoder_decode(decoder, &clone) != .success) return error.Decode;
    return clone;
}

fn restore(handle: c.Terminal, writer: *std.Io.Writer) !void {
    // The tracker returns an error if its bounded continuation was lost. Never return
    // a plausible-looking repaint when the next output bytes cannot resume correctly.
    var continuation: [65536]u8 = undefined;
    var continuation_len: usize = 0;
    if (c.continuation_buf(handle, &continuation, continuation.len, &continuation_len) != .success) return error.Continuation;
    var required: usize = 0;
    if (snapshot.encode_buf(handle, null, 0, &required) != .out_of_space or required > max_bytes) return error.Snapshot;
    const bytes = try std.heap.page_allocator.alloc(u8, required);
    defer std.heap.page_allocator.free(bytes);
    if (snapshot.encode_buf(handle, bytes.ptr, bytes.len, &required) != .success) return error.Snapshot;
    const current = try decode(bytes);
    defer c.free(current);
    const saved = try decode(bytes);
    defer c.free(saved);
    const t = c.zigTerminal(current).?;
    const s = c.zigTerminal(saved).?;
    const active = t.screens.active_key;
    try writer.writeAll("\x1bc");
    try paint(t, s, .primary, writer);
    if (t.screens.get(.alternate) != null) {
        // Each buffer's native saved cursor was serialized explicitly. Mode 47
        // selects the alternate buffer without replacing the primary saved cursor.
        try writer.writeAll("\x1b[?47h");
        try paint(t, s, .alternate, writer);
        if (active == .primary) try writer.writeAll("\x1b[?47l");
    }
    t.screens.switchTo(active);
    try state(t, writer);
    // Replay once, after every complete restoration control. Live state is untouched.
    try writer.writeAll(continuation[0..continuation_len]);
}

fn paint(t: *Terminal, saved: *Terminal, key: Key, writer: *std.Io.Writer) !void {
    t.screens.switchTo(key);
    saved.screens.switchTo(key);
    // The upstream formatter uses DECSC internally for a blank pending-wrap cell.
    // That would overwrite the independently saved cursor. Refuse this repaint
    // rather than silently corrupt the screen's later DECRC behavior.
    const cursor = &t.screens.active.cursor;
    if (cursor.pending_wrap and !cursor.page_cell.hasText()) return error.UnrepresentableCursor;
    saved.restoreCursor();
    try representable(t);
    try representable(saved);
    try state(saved, writer);
    try writer.writeAll("\x1b7");
    try neutral(writer);
    try writer.writeAll("\x1b[H");
    var content: fmt.TerminalFormatter = .init(t, .{ .emit = .vt, .trim = false });
    content.extra = .none;
    content.extra.palette = true;
    content.format(writer) catch return error.WriteFailed;
    try state(t, writer);
}

// A neutral painting state does not save/restore the cursor or switch screens.
// Do not emit CSI s: the current browser renderer treats it as cursor-save even
// after mode 69, because it does not implement horizontal margins.
fn neutral(writer: *std.Io.Writer) !void {
    try writer.writeAll("\x1b[?6l\x1b[r\x1b[4l\x1b[20l\x1b[?7h\x1b[?45l\x1b[?1045l\x1b[?2026l");
    try writer.writeAll("\x1b[0m\x1b]8;;\x1b\\\x1b[0\"q\x1b[>4;0m\x1b[=0;1u\x1b(B\x1b)B\x1b*B\x1b+B\x0f");
}

fn state(t: *Terminal, writer: *std.Io.Writer) !void {
    try neutral(writer);
    // Configure tabs and margins before enabling origin-relative coordinates.
    // The upstream formatter omits default keyboard/style state, reset above.
    var terminal_state: fmt.TerminalFormatter = .init(t, .vt);
    terminal_state.content = .none;
    terminal_state.extra = .none;
    terminal_state.extra.tabstops = true;
    terminal_state.extra.scrolling_region = true;
    terminal_state.extra.keyboard = true;
    try terminal_state.format(writer);
    // Emit defaults too: a preceding buffer's input modes may differ. Never emit
    // destructive column changes, buffer switches or cursor-save operations.
    inline for (@typeInfo(modes.Mode).@"enum".field_names, @typeInfo(modes.Mode).@"enum".field_values) |name, value| {
        const mode: modes.Mode = @fromBackingInt(value);
        if (comptime !std.mem.eql(u8, name, "132_column") and
            !std.mem.eql(u8, name, "enable_left_and_right_margin") and
            !std.mem.eql(u8, name, "alt_screen_legacy") and
            !std.mem.eql(u8, name, "alt_screen") and
            !std.mem.eql(u8, name, "save_cursor") and
            !std.mem.eql(u8, name, "alt_screen_save_cursor_clear_enter")) {
            const tag: modes.ModeTag = @bitCast(@backingInt(mode));
            try writer.print("\x1b[{s}{d}{s}", .{ if (tag.ansi) "" else "?", tag.value, if (t.modes.get(mode)) "h" else "l" });
        }
    }
    var screen_state: fmt.ScreenFormatter = .init(t.screens.active, .vt);
    screen_state.content = .none;
    screen_state.extra = .all;
    if (t.modes.get(.origin)) screen_state.cursor_origin = .{
        .x = t.scrolling_region.left, .y = t.scrolling_region.top,
    };
    try screen_state.format(writer);
    const shape: u8 = switch (t.screens.active.cursor.cursor_style) {
        .block, .block_hollow => 2,
        .underline => 4,
        .bar => 6,
    };
    try writer.print("\x1b[{d} q", .{shape - @as(u8, if (t.modes.get(.cursor_blinking)) 1 else 0)});
}

// These states have no faithful representation in the current browser renderer.
// Refuse only a requested repaint; ordinary PTY output still flows unchanged.
fn representable(t: *Terminal) !void {
    if (t.scrolling_region.left != 0 or t.scrolling_region.right != t.cols - 1 or
        t.screens.active.charset.gr != .G2 or
        t.screens.active.charset.single_shift != null) return error.UnsupportedRendererState;
}
