;; `call_indirect` across modules with two different signatures that share the same CRC32!

(module $defines
  (table (export "table") 1 funcref)
  (elem (i32.const 0) func $callee)

  (func $callee (param i64 f64 i64 i64) (result i32 i64 f64 f32 i64 i32 i64 i32 i32)
    i32.const 1
    i64.const 2
    f64.const 3
    f32.const 4
    i64.const 5
    i32.const 6
    i64.const 7
    i32.const 8
    i32.const 9))
(register "defines" $defines)

(module $calls
  (import "defines" "table" (table 1 funcref))

  (func (export "run") (result f64 f64 f32 i32 i64 i64 f64 f32 f32)
    i32.const 0  ;; argument
    i32.const 0  ;; table slot
    call_indirect (param i32) (result f64 f64 f32 i32 i64 i64 f64 f32 f32)))

(assert_trap (invoke $calls "run") "indirect call type mismatch")
