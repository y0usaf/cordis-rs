//! End-to-end proof of the spatiotemporal model through WASM:
//! mount runs effects, unmount restores pre-mount state (kernel-side inverse
//! tracking), and a committed `set` notifies exactly the declared readers.

use cordis::Context;

/// A minimal guest: `mount` sets `mode=command` and reads `theme`; `on_change`
/// records the notification under `notified`; `scratch` reserves a buffer for
/// key delivery.
///
/// Data layout: "mode"(0,4) "command"(4,7) "theme"(11,5) "notified"(16,8)
/// "yes"(24,3). Scratch buffer at offset 256, capacity 256.
const GUEST: &str = r#"
(module
  (import "host" "ctx_set" (func $ctx_set (param i32 i32 i32 i32)))
  (import "host" "ctx_read" (func $ctx_read (param i32 i32)))
  (memory (export "memory") 2)
  (data (i32.const 0) "modecommandthemenotifiedyes")

  (func (export "scratch") (result i32 i32)
    i32.const 256  i32.const 256)

  (func (export "mount")
    i32.const 0  i32.const 4  i32.const 4  i32.const 7  call $ctx_set
    i32.const 11  i32.const 5  call $ctx_read)

  (func (export "on_change") (param i32 i32)
    i32.const 16  i32.const 8  i32.const 24  i32.const 3  call $ctx_set)
)
"#;

#[test]
fn wasm_mount_revert_coeffect() {
    let wasm = wat::parse_str(GUEST).expect("valid wat");
    let mut ctx = Context::new();

    // Temporal: mount runs the effect (mode=command) and declares the read.
    let id = ctx.mount(&wasm).expect("mount");
    assert_eq!(ctx.get("mode").as_deref(), Some("command"));

    // Spatial: a committed set on the declared key notifies the reader.
    ctx.set("theme", "dark").expect("set");
    assert_eq!(ctx.get("notified").as_deref(), Some("yes"));

    // A set on an undeclared key must not notify.
    ctx.set("other", "x").expect("set");
    assert_eq!(ctx.get("notified").as_deref(), Some("yes"));

    // Temporal: unmount restores pre-mount state (kernel-side inverse replay).
    ctx.unmount(id).expect("unmount");
    assert!(!ctx.has("mode"));
    assert!(!ctx.has("notified"));
}

#[test]
fn unmount_restores_pre_existing_key() {
    let wasm = wat::parse_str(GUEST).expect("valid wat");
    let mut ctx = Context::new();

    // A key that already exists before mount must be restored, not destroyed.
    ctx.set("mode", "existing").expect("seed");
    let id = ctx.mount(&wasm).expect("mount");
    assert_eq!(ctx.get("mode").as_deref(), Some("command"));

    ctx.unmount(id).expect("unmount");
    assert_eq!(
        ctx.get("mode").as_deref(),
        Some("existing"),
        "pre-existing key must be restored"
    );
}

#[test]
fn failed_mount_rolls_back() {
    // A guest whose mount traps after a write must leave no residue.
    let bad = r#"
(module
  (import "host" "ctx_set" (func $ctx_set (param i32 i32 i32 i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "dirty")
  (func (export "scratch") (result i32 i32) i32.const 256 i32.const 256)
  (func (export "mount")
    i32.const 0  i32.const 5  i32.const 0  i32.const 5  call $ctx_set
    unreachable)
  (func (export "on_change") (param i32 i32))
)
"#;
    let wasm = wat::parse_str(bad).expect("valid wat");
    let mut ctx = Context::new();

    assert!(ctx.mount(&wasm).is_err(), "trapping mount must fail");
    assert!(!ctx.has("dirty"), "failed mount must leave no residue");
}
