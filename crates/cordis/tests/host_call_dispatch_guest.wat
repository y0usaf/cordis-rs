(module
  ;; entry host funcs used by the dispatch guest
  (import "host" "ctx_return" (func $ctx_ret (param i32 i32)))
  ;; a buffered data op (compositor set, no params) to prove guest->host
  ;; ops emitted during a call are drained via take_ops.
  (import "host" "tomoe.clear_focus" (func $op))
  (memory (export "memory") 1)
  (data (i32.const 0) "okbad")  ;; "ok"@0, "bad"@2

  (func (export "scratch") (result i32 i32) i32.const 512 i32.const 128)

  ;; Return-form entry: (ptr,len) -> (ret_ptr,ret_len). Reads the payload
  ;; byte (proving host wrote it), returns "ok" (distinct from "hello").
  (func (export "handle") (param $p i32) (param $l i32) (result i32 i32)
    call $op
    local.get $p
    i32.load8_u
    i32.const 0
    i32.ne
    if (result i32 i32)
      i32.const 0  i32.const 2
    else
      i32.const 2  i32.const 3
    end)

  ;; Void-form host: (ptr, i32) -> () delivering its result via ctx_return.
  (func (export "entry") (param i32 i32)
    call $op
    i32.const 0  i32.const 2
    call $ctx_ret)

  (func (export "mount"))
  (func (export "on_change") (param i32 i32))
)
