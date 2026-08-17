//! Extension-surface proof (docs/abi.md sets 2, 3, 5):
//! - a guest registers extension units as *effects* that are reverted on
//!   unmount (kernel-owned reversion via the per-plugin registry),
//! - a guest emits data ops (draw + compositor) the host drains after a clean
//!   return via `Context::take_ops`,
//! - bad guest input (negative/out-of-bounds pointers) traps, never panics.

use cordis::Context;

// Guest `mount`:
//   - registers a command "c1" (set 2),
//   - registers a surface "s1" with dock/priority/size descriptors (set 2),
//   - calls the value-returning `size()` accessor (set 3),
//   - buffers draw ops `fill_rect` and `put_text` (set 3),
//   - buffers a compositor op `tomoe.bind` (set 5).
const GUEST: &str = include_str!("extension_guest.wat");

#[test]
fn registrations_are_effects_reverted_on_unmount() {
    let wasm = wat::parse_str(GUEST).expect("valid wat");
    let mut ctx = Context::new();

    let id = ctx.mount(&wasm).expect("mount");
    let regs = ctx.registrations(id).expect("registrations");
    let kinds: Vec<&str> = regs.iter().map(|(_, k, _)| k.as_str()).collect();
    let names: Vec<&str> = regs.iter().map(|(n, _, _)| n.as_str()).collect();
    assert_eq!(names, vec!["c1", "s1"], "registered units, in order");
    assert_eq!(
        kinds,
        vec!["command", "surface"],
        "each unit carries its kind"
    );

    // Temporal: unmount reverts registrations wholesale.
    ctx.unmount(id).expect("unmount");
    assert_eq!(
        ctx.registrations(id).expect("after unmount"),
        Vec::<(String, String, Vec<String>)>::new(),
        "registration registry is empty after unmount"
    );
}

#[test]
fn registrations_retain_descriptor_args() {
    // A guest that registers a mode-scoped keybinding and a surface with
    // scalars: the descriptors after the name must survive registration so
    // the host can reconstruct them (the pre-widening kernel kept only the
    // first field).
    let guest = r#"
(module
  (import "host" "register_keybinding" (func $reg_key (param i32 i32 i32 i32 i32 i32 i32 i32)))
  (import "host" "register_surface" (func $reg_surf (param i32 i32 i32 i32 i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "jleaderdesc")
  (func (export "scratch") (result i32 i32) i32.const 256 i32.const 256)
  (func (export "mount")
    ;; register_keybinding("j", "leader", "desc", "handler")
    i32.const 0 i32.const 1
    i32.const 1 i32.const 6
    i32.const 7 i32.const 4
    i32.const 0 i32.const 1
    call $reg_key
    ;; register_surface("s1", dock=1, priority=0, size=10)
    i32.const 0 i32.const 1
    i32.const 1 i32.const 0 i32.const 10
    call $reg_surf)
  (func (export "on_change") (param i32 i32))
)
"#;
    let wasm = wat::parse_str(guest).expect("valid wat");
    let mut ctx = Context::new();
    let id = ctx.mount(&wasm).expect("mount");
    let regs = ctx.registrations(id).expect("registrations");

    // Keybinding: name "j", kind "keybinding", mode descriptor "leader".
    assert_eq!(regs[0].0, "j");
    assert_eq!(regs[0].1, "keybinding");
    assert_eq!(regs[0].2, vec!["leader", "desc", "j"]);

    // Surface: name "s1" (reads offset 0 = "j"), kind "surface", scalar
    // descriptors as strings.
    assert_eq!(regs[1].0, "j");
    assert_eq!(regs[1].1, "surface");
    assert_eq!(regs[1].2, vec!["1", "0", "10"]);
}

#[test]
fn ops_are_drained_after_clean_return() {
    let wasm = wat::parse_str(GUEST).expect("valid wat");
    let mut ctx = Context::new();

    let id = ctx.mount(&wasm).expect("mount");
    let ops = ctx.take_ops(id).expect("drain ops");

    assert_eq!(
        ops.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
        vec!["fill_rect", "put_text", "tomoe.bind"],
        "draw + compositor ops buffered, in emit order"
    );
    assert_eq!(ops[0].1, vec!["0", "0", "10", "10", "#fff"]);
    assert_eq!(ops[1].1, vec!["0", "0", "hi", "blue"]);
    assert_eq!(ops[2].1, vec!["C-S", "action", "d"]);

    // A clean return leaves an empty buffer (fully drained).
    assert_eq!(
        ctx.take_ops(id).expect("drain again"),
        Vec::<(String, Vec<String>)>::new()
    );
}

#[test]
fn bad_input_traps_not_panics() {
    // Guest passes len=999 (out of the 2-page memory) to `register_command`.
    let bad = r#"
(module
  (import "host" "register_command" (func $reg_cmd (param i32 i32 i32 i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "x")
  (func (export "scratch") (result i32 i32) i32.const 256 i32.const 256)
  (func (export "mount")
    i32.const 0 i32.const 1   ;; name "x"
    i32.const 1 i32.const 1000000 ;; desc: len far past 1-page memory -> trap
    call $reg_cmd)
  (func (export "on_change") (param i32 i32))
)
"#;
    let wasm = wat::parse_str(bad).expect("valid wat");
    let mut ctx = Context::new();

    // Bad input must produce a trapped mount, never a Rust panic.
    let err = ctx.mount(&wasm).expect_err("bad pointer/len must trap");
    assert!(!err.to_string().is_empty());
    // A failed mount must leave no registry residue.
    assert_eq!(
        ctx.registrations(0)
            .expect("rolled-back plugin still addressable"),
        Vec::<(String, String, Vec<String>)>::new(),
        "failed mount must reverted-register nothing"
    );
}
