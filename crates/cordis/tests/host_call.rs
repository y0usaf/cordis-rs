//! Dynamic host->guest dispatch proof (`docs/abi.md` set 6, the restored
//! dynamic-callback surface): the host calls an arbitrary guest export with a
//! payload string and reads a distinct result string back, guest->host ops
//! emitted during the call are drained via `take_ops`, and a guest that
//! overruns the fuel budget traps cleanly (no panic, no state residue).

use cordis::Context;

const GUEST: &str = include_str!("host_call_dispatch_guest.wat");

#[test]
fn call_dispatch_payload_in_result_out() {
    // (a) the host calls a guest export with a payload string and receives a
    // distinct result string back.
    let wasm = wat::parse_str(GUEST).expect("valid wat");
    let mut ctx = Context::new();
    let id = ctx.mount(&wasm).expect("mount");

    let result = ctx.call(id, "handle", "hello").expect("call guest export");
    assert_eq!(result, "ok", "guest read the payload and returned 'ok'");
    assert_ne!(result, "hello", "result is distinct from the payload");
}

#[test]
fn call_drains_ops_emitted_during_call() {
    // (b) guest->host ops emitted during the call are drained + visible via
    // take_ops. The dispatch guest calls `tomoe.clear_focus` inside an entry.
    let wasm = wat::parse_str(GUEST).expect("valid wat");
    let mut ctx = Context::new();
    let id = ctx.mount(&wasm).expect("mount");

    let _ = ctx.call(id, "handle", "hello").expect("call");
    let ops = ctx.take_ops(id).expect("drain ops");
    assert_eq!(
        ops.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
        vec!["tomoe.clear_focus"],
        "op emitted during the call is drained"
    );
    // A second call emits fresh ops, not a mix with the first call.
    let _ = ctx.call(id, "entry", "again").expect("call");
    let ops = ctx.take_ops(id).expect("drain again");
    assert_eq!(ops.len(), 1, "each call drains only its own ops");
}

#[test]
fn call_void_form_result_via_ctx_return() {
    // A guest whose export is (ptr,len)->() delivers its result through the
    // host.ctx_return host function; the host reads it back.
    let wasm = wat::parse_str(GUEST).expect("valid wat");
    let mut ctx = Context::new();
    let id = ctx.mount(&wasm).expect("mount");

    let result = ctx.call(id, "entry", "ignored").expect("call void entry");
    assert_eq!(result, "ok", "void-form result arrives via ctx_return");
}

#[test]
fn call_single_value_scratch_form() {
    // A Rust-authored guest can't emit a `(i32,i32)` return (rustc lowers it
    // to C sret on wasm32-unknown-unknown), so `scratch() -> i32` is also
    // accepted: the value is the buffer base and a fixed 64 KiB capacity is
    // derived. The entry still returns (ret_ptr, ret_len) into memory.
    let guest = r#"(module
  (memory (export "memory") 2)
  ;; scratch returns a single pointer into memory (offset 0). Result written
  ;; back at the same region by the export, which echoes a payload length.
  (func (export "scratch") (result i32) i32.const 0)
  (func (export "mount"))
  (func (export "entry") (param i32 i32) (result i32 i32)
    ;; copy "OK\x00" into memory at offset 8 and return (8, 2)
    (i32.store8 (i32.const 8) (i32.const 79))   ;; O
    (i32.store8 (i32.const 9) (i32.const 75))   ;; K
    i32.const 8 i32.const 2)
  (func (export "on_change") (param i32 i32))
)"#;
    let wasm = wat::parse_str(guest).expect("valid wat");
    let mut ctx = Context::new();
    let id = ctx.mount(&wasm).expect("mount");
    let result = ctx.call(id, "entry", "any-payload").expect("call");
    assert_eq!(
        result, "OK",
        "single-value scratch form delivers the result"
    );
}

#[test]
fn runaway_guest_traps_cleanly_no_residue() {
    // (c) a guest that overruns the fuel budget traps (not panic) and leaves
    // no state residue; the plugin stays mounted and the store recovers.
    let runaway = r#"(module
  (memory (export "memory") 1)
  (func (export "mount"))
  (func (export "scratch") (result i32 i32) i32.const 512 i32.const 128)
  (func (export "loop") (param i32 i32)
    (local i32)
    i32.const 0
    local.set 0
    (loop $l
      local.get 0
      i32.const 1
      i32.add
      local.set 0
      br $l))
  (func (export "on_change") (param i32 i32))
)"#;
    let wasm = wat::parse_str(runaway).expect("valid wat");
    let mut ctx = Context::new();
    let id = ctx.mount(&wasm).expect("mount");

    let err = ctx
        .call(id, "loop", "x")
        .expect_err("fuel exhaustion traps");
    assert!(
        matches!(
            err.downcast_ref::<wasmtime::Trap>(),
            Some(wasmtime::Trap::OutOfFuel)
        ),
        "budget overrun must be a clean fuel-exhaustion trap, got: {err}"
    );

    // No residue: ops are empty; plugin still mounted and addressable.
    assert_eq!(
        ctx.take_ops(id).expect("empty ops after trap"),
        Vec::<(String, Vec<String>)>::new(),
        "trap leaves no ops residue"
    );

    // The store recovers with a fresh fuel budget for the next call.
    let again = ctx.call(id, "loop", "x");
    assert!(again.is_err(), "loop still traps");
}
