(module
  (import "host" "ctx_set" (func $ctx_set (param i32 i32 i32 i32)))
  (import "host" "ctx_read" (func $ctx_read (param i32 i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "themedarkkeymapvimcolorsbase16")

  (func (export "scratch") (result i32 i32)
    i32.const 256  i32.const 256)

  (func (export "mount")
    i32.const 0  i32.const 5  i32.const 5  i32.const 4  call $ctx_set
    i32.const 9  i32.const 6  i32.const 15  i32.const 3  call $ctx_set
    i32.const 18  i32.const 6  i32.const 24  i32.const 6  call $ctx_set
    i32.const 0  i32.const 5  call $ctx_read
    i32.const 9  i32.const 6  call $ctx_read)

  (func (export "on_change") (param i32 i32))
)
