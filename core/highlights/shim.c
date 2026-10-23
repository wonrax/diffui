// The C face of the wasm2c-translated highlights module: Rust sees an opaque
// handle and plain pointers. A trap longjmps back to `hl_highlight`, so the
// unwind never crosses into Rust frames.
#include <stdint.h>
#include <stdlib.h>

#include "highlights.h"
#include "wasm-rt-exceptions.h"
#include "wasm-rt-impl.h"

struct w2c_env {
  w2c_highlights* inst;
};

typedef struct {
  struct w2c_env env;
  w2c_highlights inst;
} hl_t;

// Unicode identifier classes for the ECMAScript lexers, answered in Rust.
extern uint32_t diffui_highlights_is_id(const uint8_t* bytes, uint32_t len, uint32_t cont);

static uint32_t is_id(struct w2c_env* env, u32 ptr, u32 len, uint32_t cont) {
  wasm_rt_memory_t* mem = w2c_highlights_memory(env->inst);
  if (len == 0 || len > 4 || (uint64_t)ptr + len > mem->size) return 0;
  return diffui_highlights_is_id(mem->data + ptr, len, cont);
}

u32 w2c_env_is_id_start(struct w2c_env* env, u32 ptr, u32 len) {
  return is_id(env, ptr, len, 0);
}

u32 w2c_env_is_id_continue(struct w2c_env* env, u32 ptr, u32 len) {
  return is_id(env, ptr, len, 1);
}

hl_t* diffui_hl_new(void) {
  wasm_rt_init();
  hl_t* hl = calloc(1, sizeof(hl_t));
  if (!hl) return NULL;
  hl->env.inst = &hl->inst;
  wasm2c_highlights_instantiate(&hl->inst, &hl->env);
  return hl;
}

void diffui_hl_free(hl_t* hl) {
  if (!hl) return;
  wasm2c_highlights_free(&hl->inst);
  free(hl);
}

uint8_t* diffui_hl_memory(hl_t* hl, uint64_t* len) {
  wasm_rt_memory_t* mem = w2c_highlights_memory(&hl->inst);
  *len = mem->size;
  return mem->data;
}

// 0 on success, nonzero when the memory cannot grow.
int diffui_hl_grow(hl_t* hl, uint32_t pages) {
  return wasm_rt_grow_memory(w2c_highlights_memory(&hl->inst), pages) == (uint64_t)-1;
}

// 0, or the wasm_rt trap code.
int diffui_hl_highlight(hl_t* hl) {
  wasm_rt_trap_t trap = wasm_rt_impl_try();
  if (trap != 0) return (int)trap;
  w2c_highlights_highlight(&hl->inst);
  return 0;
}
