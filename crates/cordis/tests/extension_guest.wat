(module
  ;; ekko registration (set 2)
  (import "host" "register_command" (func $reg_cmd (param i32 i32 i32 i32)))
  (import "host" "register_surface" (func $reg_surf (param i32 i32 i32 i32 i32)))
  ;; draw ops (set 3)
  (import "host" "fill_rect" (func $fill (param i32 i32 i32 i32 i32 i32 i32 i32 i32 i32)))
  (import "host" "put_text" (func $text (param i32 i32 i32 i32 i32 i32 i32 i32)))
  (import "host" "size" (func $size (result i32 i32)))
  ;; compositor ops (set 5)
  (import "host" "tomoe.bind" (func $bind (param i32 i32 i32 i32 i32 i32)))
  (memory (export "memory") 2)
  (data (i32.const 0) "\63\31\63\6f\6d\6d\61\6e\64\20\64\65\73\63\73\31\30\30\31\30\31\30\23\66\66\66\68\69\62\6c\75\65\43\2d\53\61\63\74\69\6f\6e\64")

  (func (export "scratch") (result i32 i32) i32.const 256 i32.const 256)

  (func (export "mount")
    ;; register_command("c1","command desc")
    i32.const 0 i32.const 2
    i32.const 2 i32.const 12
    call $reg_cmd
    ;; register_surface("s1", dock=1, priority=0, size=10)
    i32.const 14 i32.const 2
    i32.const 1 i32.const 0 i32.const 10
    call $reg_surf
    ;; size() -> (w,h) (value-returning accessor)
    call $size drop drop
    ;; fill_rect("0","0","10","10","#fff")
    i32.const 16 i32.const 1
    i32.const 17 i32.const 1
    i32.const 18 i32.const 2
    i32.const 20 i32.const 2
    i32.const 22 i32.const 4
    call $fill
    ;; put_text("0","0","hi","blue")
    i32.const 16 i32.const 1
    i32.const 17 i32.const 1
    i32.const 26 i32.const 2
    i32.const 28 i32.const 4
    call $text
    ;; tomoe.bind("C-S","action","d")
    i32.const 32 i32.const 3
    i32.const 35 i32.const 6
    i32.const 41 i32.const 1
    call $bind)

  (func (export "on_change") (param i32 i32))
)
