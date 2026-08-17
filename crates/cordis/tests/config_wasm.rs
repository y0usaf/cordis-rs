//! Reference pattern: config ships as a precompiled WASM module loaded at
//! startup. The host reads the compiled config bytes once, compiles them, and
//! mounts the config extension BEFORE any other extension. Config is an
//! extension that only WRITES: its `mount` calls `ctx_set` to populate config
//! keys (`theme`, `keymap`, `colors`) and `ctx_read` to declare reads.
//!
//! There is NO text config format and NO config parser — config IS data, as a
//! WASM module using the same host ABI (`ctx_set`/`ctx_remove`) as any other
//! extension ([[principle:no-privileged-path]]).
//!
//! Unmounting the config reverts the context to its pre-mount state with no
//! residue (kernel-side inverse replay; [[principle:spatiotemporal]]).

use cordis::Context;

/// The config module, as source bytes. "Read once at startup": included at
/// compile time, compiled to a module once, mounted before any extension.
const CONFIG_WAT: &str = include_str!("config_wasm.wat");

#[test]
fn config_wasm_loads_at_startup_and_sets_keys() {
    // Startup: the host reads the compiled config bytes once and compiles them.
    let config_wasm = wat::parse_str(CONFIG_WAT).expect("valid config wat");

    let mut ctx = Context::new();

    // Config mounts FIRST — before any extension — and only writes.
    let config_id = ctx.mount(&config_wasm).expect("mount config at startup");
    assert_eq!(ctx.get("theme").as_deref(), Some("dark"));
    assert_eq!(ctx.get("keymap").as_deref(), Some("vim"));
    assert_eq!(ctx.get("colors").as_deref(), Some("base16"));

    // A consumer extension mounts after config and sees the config keys.
    let consumer_wasm = wat::parse_str(WAT_CONSUMER).expect("valid consumer wat");
    let consumer = ctx
        .mount(&consumer_wasm)
        .expect("consumer mounts after config");
    assert_eq!(ctx.get("theme").as_deref(), Some("dark"));
    ctx.unmount(consumer).expect("consumer unmount");
    assert!(ctx.has("theme"), "consumer must not remove config keys");

    // Unmount config -> context reverts to pre-mount (config-as-wasm has no
    // residue).
    ctx.unmount(config_id).expect("unmount config");
    assert!(!ctx.has("theme"), "config key must revert on unmount");
    assert!(!ctx.has("keymap"), "config key must revert on unmount");
    assert!(!ctx.has("colors"), "config key must revert on unmount");
}

#[test]
fn config_wasm_reverts_pre_existing_keys() {
    let config_wasm = wat::parse_str(CONFIG_WAT).expect("valid config wat");
    let mut ctx = Context::new();

    // A key already present before startup must be restored, not destroyed.
    ctx.set("theme", "existing").expect("seed");
    let id = ctx.mount(&config_wasm).expect("mount config");
    assert_eq!(ctx.get("theme").as_deref(), Some("dark"));

    ctx.unmount(id).expect("unmount config");
    assert_eq!(
        ctx.get("theme").as_deref(),
        Some("existing"),
        "pre-existing key must be restored on config unmount"
    );
}

/// A minimal consumer extension mounted after config; declares a read on
/// `theme` (coeffect) so it would be notified on a committed change.
const WAT_CONSUMER: &str = r#"
(module
  (import "host" "ctx_read" (func $ctx_read (param i32 i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "theme")
  (func (export "scratch") (result i32 i32) i32.const 256 i32.const 256)
  (func (export "mount") i32.const 0 i32.const 5 call $ctx_read)
  (func (export "on_change") (param i32 i32))
)
"#;
