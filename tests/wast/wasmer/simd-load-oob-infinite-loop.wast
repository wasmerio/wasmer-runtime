;; https://github.com/wasmerio/wasmer/issues/5665
;; An out-of-bounds v128.load32x2_s whose result is unused must still trap,
;; even though it sits in an infinite loop.

(module
  (type (;0;) (func (result i32 i64 f32 f64 v128 v128 v128 v128)))
  (memory (;0;) 0)
  (global (;0;) i64 i64.const 0)
  (global (;1;) (mut v128) v128.const i32x4 0x00000000 0x00000000 0x00000000 0x00000000)
  (global (;2;) (mut i64) i64.const 0)
  (global (;3;) (mut i64) i64.const 0)
  (global (;4;) (mut i32) i32.const 0)
  (global (;5;) (mut i32) i32.const 0)
  (export "main" (func 0))
  (func (;0;) (type 0) (result i32 i64 f32 f64 v128 v128 v128 v128)
    (local v128 f32 v128)
    i64.const 1
    global.get 0
    block (type 0) (result i32 i64 f32 f64 v128 v128 v128 v128) ;; label = @1
      loop ;; label = @2
        f64.const 0x1.ba5288904c7e4p-891
        i32.const 0
        loop ;; label = @3
          memory.size
          v128.load32x2_s align=4
          i8x16.all_true
          br 0 (;@3;)
          i64.const 1
          i64.const 0
          block (result f32) ;; label = @4
            call 0
            local.tee 0
            f32x4.mul
            local.tee 2
            v128.const i32x4 0x7fc00000 0x7fc00000 0x7fc00000 0x7fc00000
            local.get 2
            local.get 2
            f32x4.eq
            v128.bitselect
            global.get 1
            v128.xor
            global.set 1
            global.get 1
            v128.xor
            global.set 1
            global.get 1
            v128.xor
            global.set 1
            i64.reinterpret_f64
            global.get 2
            i64.xor
            global.set 2
            drop
            global.get 3
            i64.xor
            global.set 3
            global.get 4
            i32.xor
            global.set 4
            f32.const -0x1.fffffep+127
          end
          i32.reinterpret_f32
          global.get 5
          i32.xor
          global.set 5
          global.get 3
          i64.xor
          global.set 3
          global.get 3
          i64.xor
          global.set 3
          global.get 4
          i32.xor
          global.set 4
        end
        global.get 4
        i32.xor
        global.set 4
        i64.reinterpret_f64
        global.get 2
        i64.xor
        global.set 2
      end
      i32.const 0
      i64.const 0
      f32.const -0x1.03ea9ap+109
      f64.const -0x1.d0670aaf7b65bp-784
      v128.const i32x4 0x00000000 0x00002000 0x07ffffff 0x00000000
      v128.const i32x4 0xffffffff 0xffffdfff 0x00002000 0x00000000
      v128.const i32x4 0x08000000 0x00000000 0xffffffff 0xf7ffffff
      v128.const i32x4 0x3fffffff 0x00000000 0xffffffff 0xffffffdf
    end
    global.get 1
    v128.xor
    global.set 1
    global.get 1
    v128.xor
    global.set 1
    global.get 1
    v128.xor
    global.set 1
    global.get 1
    v128.xor
    global.set 1
    i64.reinterpret_f64
    global.get 2
    i64.xor
    global.set 2
    i32.reinterpret_f32
    global.get 5
    i32.xor
    global.set 5
    global.get 3
    i64.xor
    global.set 3
    global.get 4
    i32.xor
    global.set 4
    global.get 3
    i64.xor
    global.set 3
    global.get 3
    i64.xor
    global.set 3
    i32.const 0
    i64.const 1
    f32.const -0x1.89b576p+28
    f64.const 0x1.8722f2240dc3bp+1008
    v128.const i32x4 0xdd8a75be 0xd4779741 0x42010013 0x0a8994d1
    v128.const i32x4 0x36701c6a 0xecbd09f7 0x0fe6322d 0x9d8d1db6
    v128.const i32x4 0x5e8fd469 0xacf03f39 0x76615e28 0xb19d5420
    v128.const i32x4 0xa8e7e036 0x4e960d3e 0x90a5f7fb 0x5477f442
  )
)

(assert_trap (invoke "main") "out of bounds memory access")
