/*
 * fakellm <-> txiki.js glue. See fakellm_tjs.h for the contract.
 *
 * All policy-facing logic (JSON in and out, the ctx object, decline handling, log capture) is JavaScript, evaluated once per runtime from
 * WRAPPER_JS below; it mirrors the node/bun/tjs shims in src/policy. The C side only creates the runtime, drives the event loop until a
 * promise settles, enforces the deadline and moves strings across.
 */
#include "fakellm_tjs.h"

#include "private.h"

#include <stdatomic.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* Keep this free of // comments and of lone quotes: the preprocessor flattens it to one line. Statements end in semicolons. */
#define JS(...) #__VA_ARGS__

static const char WRAPPER_JS[] = JS(
(function () {
  "use strict";
  const logs = [];
  const MAX_LINES = 1000;
  const MAX_LINE = 8192;
  let dropped = 0;
  const text = (a) => {
    if (typeof a === "string") return a;
    try {
      const s = JSON.stringify(a);
      return s === undefined ? String(a) : s;
    } catch (e) {
      return String(a);
    }
  };
  const log = (...args) => {
    if (logs.length >= MAX_LINES) { dropped++; return; }
    const line = args.map(text).join(" ");
    logs.push(line.length > MAX_LINE ? line.slice(0, MAX_LINE) + "... (truncated)" : line);
  };
  console.log = console.info = console.debug = console.warn = console.error = log;
  const describe = (e) => (e instanceof Error ? e.message : String(e));
  const describeLoad = (e) => {
    if (e instanceof SyntaxError && typeof e.stack === "string") {
      const frame = e.stack.split("\n").find((line) => line.trim().startsWith("at "));
      if (frame) {
        const text = frame.trim();
        const open = text.lastIndexOf("(");
        return e.message + " (" + (open >= 0 ? text.slice(open + 1, text.lastIndexOf(")")) : text.slice(3)) + ")";
      }
    }
    return describe(e);
  };
  let policy;
  return {
    drain() {
      const out = logs.splice(0, logs.length);
      if (dropped > 0) { out.push("... " + dropped + " more log lines dropped"); dropped = 0; }
      return out;
    },
    async load(path) {
      try {
        const mod = await import(path);
        if (typeof mod.default !== "function") {
          throw new Error("the module must export a default function decide(request, context)");
        }
        policy = mod.default;
        return {};
      } catch (e) {
        return { error: describeLoad(e) };
      }
    },
    async decide(requestJson, stateJson, hits) {
      try {
        const request = JSON.parse(requestJson);
        const state = JSON.parse(stateJson);
        request.tools = request.tools ?? [];
        request.stop = request.stop ?? [];
        const out = await policy(request, { state, hits, log });
        if (out === null || out === undefined || (typeof out === "object" && !Array.isArray(out) && out.decline === true)) {
          const kept = out ? out.state : undefined;
          return { decline: true, state: kept === undefined ? undefined : JSON.stringify(kept) };
        }
        if (typeof out !== "object" || Array.isArray(out)) {
          throw new Error("decide must return an object (a ReplySpec, optionally with a `state` field), null or {decline: true}");
        }
        const { state: next, ...reply } = out;
        return { reply: JSON.stringify(reply), state: next === undefined ? undefined : JSON.stringify(next) };
      } catch (e) {
        return { error: describe(e) };
      }
    },
  };
})()
);

struct fakellm_tjs {
    TJSRuntime *qrt;
    JSRuntime *rt;
    JSContext *ctx;
    JSValue api; /* the object WRAPPER_JS evaluates to */
    uv_timer_t timer; /* wakes the loop at the deadline */
    uv_async_t wake; /* wakes the loop when another thread cancels */
    atomic_bool cancelled; /* fakellm_tjs_cancel was called; sticky */
    uint64_t deadline_ns; /* 0 = none; uv_hrtime() clock */
    bool interrupted; /* the interrupt handler fired */
    bool broken; /* a call timed out; the runtime may hold a stuck promise */
    fakellm_tjs_log_fn log_fn;
    void *log_user;
};

static uv_once_t global_once = UV_ONCE_INIT;

static void global_init(void) {
    /* Process-wide txiki.js setup (libuv allocator, SIGPIPE). The buffer is static because libuv keeps pointing into it. */
    static char arg0[] = "fakellm";
    static char *argv[] = { arg0, NULL };
    TJS_Initialize(1, argv);
}

static int interrupt_cb(JSContext *ctx, void *opaque) {
    (void) ctx;
    fakellm_tjs *h = opaque;
    if (atomic_load(&h->cancelled)) {
        h->interrupted = true;
        return 1;
    }
    if (h->deadline_ns != 0 && uv_hrtime() >= h->deadline_ns) {
        h->interrupted = true;
        return 1;
    }
    return 0;
}

static void timer_cb(uv_timer_t *timer) {
    (void) timer;
}

static void wake_cb(uv_async_t *async) {
    (void) async;
}

static char *dup_cstr(const char *s) {
    size_t n = strlen(s) + 1;
    char *r = malloc(n);
    if (r) {
        memcpy(r, s, n);
    }
    return r;
}

static char *dup_js_string(JSContext *ctx, JSValueConst v) {
    const char *s = JS_ToCString(ctx, v);
    if (!s) {
        JS_FreeValue(ctx, JS_GetException(ctx));
        return dup_cstr("(unprintable value)");
    }
    char *r = dup_cstr(s);
    JS_FreeCString(ctx, s);
    return r;
}

/* Takes the pending exception and renders it: the message for Error objects, the string form otherwise. */
static char *take_exception(JSContext *ctx) {
    JSValue ex = JS_GetException(ctx);
    char *r = NULL;
    if (JS_IsObject(ex)) {
        JSValue message = JS_GetPropertyStr(ctx, ex, "message");
        if (JS_IsString(message)) {
            r = dup_js_string(ctx, message);
        }
        JS_FreeValue(ctx, message);
    }
    if (!r) {
        r = dup_js_string(ctx, ex);
    }
    JS_FreeValue(ctx, ex);
    return r;
}

static void set_error(char **err_out, char *message) {
    if (err_out) {
        *err_out = message;
    } else {
        free(message);
    }
}

static void emit_log(fakellm_tjs *h, const char *line, size_t len) {
    if (h->log_fn) {
        h->log_fn(h->log_user, line, len);
    } else {
        fprintf(stderr, "%.*s\n", (int) len, line);
    }
}

static void drain_logs(fakellm_tjs *h) {
    JSContext *ctx = h->ctx;
    JSValue drain = JS_GetPropertyStr(ctx, h->api, "drain");
    JSValue lines = JS_Call(ctx, drain, h->api, 0, NULL);
    JS_FreeValue(ctx, drain);
    if (JS_IsException(lines)) {
        JS_FreeValue(ctx, JS_GetException(ctx));
        return;
    }
    JSValue length = JS_GetPropertyStr(ctx, lines, "length");
    uint32_t n = 0;
    JS_ToUint32(ctx, &n, length);
    JS_FreeValue(ctx, length);
    for (uint32_t i = 0; i < n; i++) {
        JSValue item = JS_GetPropertyUint32(ctx, lines, i);
        size_t len;
        const char *s = JS_ToCStringLen(ctx, &len, item);
        if (s) {
            emit_log(h, s, len);
            JS_FreeCString(ctx, s);
        }
        JS_FreeValue(ctx, item);
    }
    JS_FreeValue(ctx, lines);
}

typedef enum { PUMP_SETTLED, PUMP_TIMEOUT, PUMP_STALLED, PUMP_FAILED } pump_result;

/*
 * Runs microtasks and the libuv loop until `promise` settles. This is TJS_Run without the entry point: tjs__execute_jobs is what txiki.js
 * runs from its check handle after every loop iteration (microtasks, then unhandled-rejection processing).
 */
static pump_result pump(fakellm_tjs *h, JSValueConst promise) {
    uv_loop_t *loop = TJS_GetLoop(h->qrt);
    for (;;) {
        tjs__execute_jobs(h->ctx);
        if (h->interrupted || atomic_load(&h->cancelled)) {
            return PUMP_TIMEOUT;
        }
        if (JS_HasException(h->ctx)) {
            return PUMP_FAILED;
        }
        if (JS_PromiseState(h->ctx, promise) != JS_PROMISE_PENDING) {
            return PUMP_SETTLED;
        }
        uint64_t now = uv_hrtime();
        if (h->deadline_ns != 0 && now >= h->deadline_ns) {
            return PUMP_TIMEOUT;
        }
        if (!JS_IsJobPending(h->rt) && !uv_loop_alive(loop)) {
            return PUMP_STALLED;
        }
        if (h->deadline_ns != 0) {
            uint64_t remaining_ms = (h->deadline_ns - now + 999999) / 1000000;
            uv_timer_start(&h->timer, timer_cb, remaining_ms, 0);
        }
        uv_run(loop, UV_RUN_ONCE);
        uv_timer_stop(&h->timer);
    }
}

/*
 * Calls api.<method>(...argv) and drives the loop until its promise settles. On OK, *result is the fulfilled value (owned by the caller).
 * The JS methods catch everything themselves, so a rejection here is a bug and reported as an error.
 */
static int run(fakellm_tjs *h, const char *method, int argc, JSValueConst *argv, int timeout_ms, JSValue *result, char **err_out) {
    JSContext *ctx = h->ctx;
    int status = FAKELLM_TJS_ERROR;

    if (atomic_load(&h->cancelled)) {
        return FAKELLM_TJS_CANCELLED;
    }
    if (h->broken) {
        set_error(err_out, dup_cstr("the JavaScript runtime is unusable after a timeout; create a new one"));
        return FAKELLM_TJS_ERROR;
    }

    /* The handle may have moved to another thread since the last call: re-anchor QuickJS's stack check. */
    JS_UpdateStackTop(h->rt);
    h->interrupted = false;
    h->deadline_ns = timeout_ms > 0 ? uv_hrtime() + (uint64_t) timeout_ms * 1000000u : 0;

    JSValue fn = JS_GetPropertyStr(ctx, h->api, method);
    JSValue promise = JS_Call(ctx, fn, h->api, argc, argv);
    JS_FreeValue(ctx, fn);

    if (JS_IsException(promise)) {
        char *message = take_exception(ctx);
        if (h->interrupted) {
            free(message);
            status = FAKELLM_TJS_TIMEOUT;
        } else {
            set_error(err_out, message);
        }
    } else {
        switch (pump(h, promise)) {
            case PUMP_SETTLED:
                if (JS_PromiseState(ctx, promise) == JS_PROMISE_FULFILLED) {
                    *result = JS_PromiseResult(ctx, promise);
                    status = FAKELLM_TJS_OK;
                } else {
                    JSValue reason = JS_PromiseResult(ctx, promise);
                    set_error(err_out, dup_js_string(ctx, reason));
                    JS_FreeValue(ctx, reason);
                }
                break;
            case PUMP_TIMEOUT:
                /* A runaway loop was interrupted with an uncatchable exception; clear it. */
                if (JS_HasException(ctx)) {
                    JS_FreeValue(ctx, JS_GetException(ctx));
                }
                status = FAKELLM_TJS_TIMEOUT;
                break;
            case PUMP_FAILED: {
                /* An uncaught exception in a callback or an unhandled promise rejection stopped the loop. */
                char *message = take_exception(ctx);
                if (h->interrupted) {
                    free(message);
                    status = FAKELLM_TJS_TIMEOUT;
                } else {
                    set_error(err_out, message);
                }
                break;
            }
            case PUMP_STALLED:
                set_error(err_out, dup_cstr("the policy returned a promise that can never settle (nothing is pending on the event loop)"));
                break;
        }
    }
    JS_FreeValue(ctx, promise);

    h->deadline_ns = 0;
    if (status == FAKELLM_TJS_TIMEOUT) {
        h->broken = true;
        if (atomic_load(&h->cancelled)) {
            status = FAKELLM_TJS_CANCELLED;
        } else {
            set_error(err_out, dup_cstr("timed out"));
        }
    }
    drain_logs(h);
    return status;
}

fakellm_tjs *fakellm_tjs_new(int mem_limit, size_t stack_size) {
    uv_once(&global_once, global_init);

    TJSRunOptions options;
    TJS_DefaultOptions(&options);
    if (mem_limit > 0) {
        options.mem_limit = mem_limit;
    }
    if (stack_size > 0) {
        options.stack_size = stack_size;
    }

    TJSRuntime *qrt = TJS_NewRuntimeOptions(&options);
    if (!qrt) {
        return NULL;
    }
    fakellm_tjs *h = calloc(1, sizeof(*h));
    if (!h) {
        TJS_FreeRuntime(qrt);
        return NULL;
    }
    h->qrt = qrt;
    h->rt = JS_GetRuntime(TJS_GetJSContext(qrt));
    h->ctx = TJS_GetJSContext(qrt);
    h->api = JS_UNDEFINED;

    /* We never call TJS_Run, so nothing else would unref this handle and the loop would count as alive forever. */
    uv_unref((uv_handle_t *) &qrt->stop);
    uv_loop_t *loop = TJS_GetLoop(qrt);
    uv_timer_init(loop, &h->timer);
    uv_unref((uv_handle_t *) &h->timer);
    uv_async_init(loop, &h->wake, wake_cb);
    uv_unref((uv_handle_t *) &h->wake);
    JS_SetInterruptHandler(h->rt, interrupt_cb, h);

    h->api = JS_Eval(h->ctx, WRAPPER_JS, sizeof(WRAPPER_JS) - 1, "<fakellm>", JS_EVAL_TYPE_GLOBAL);
    if (JS_IsException(h->api)) {
        char *message = take_exception(h->ctx);
        fprintf(stderr, "fakellm_tjs: cannot initialize the policy wrapper: %s\n", message);
        free(message);
        h->api = JS_UNDEFINED;
        fakellm_tjs_free(h);
        return NULL;
    }
    return h;
}

void fakellm_tjs_cancel(fakellm_tjs *h) {
    atomic_store(&h->cancelled, true);
    uv_async_send(&h->wake);
}

void fakellm_tjs_set_log(fakellm_tjs *h, fakellm_tjs_log_fn fn, void *user) {
    h->log_fn = fn;
    h->log_user = user;
}

int fakellm_tjs_load(fakellm_tjs *h, const char *dir, const char *entry, int timeout_ms, char **err_out) {
    if (err_out) {
        *err_out = NULL;
    }
    size_t dir_len = strlen(dir);
    while (dir_len > 1 && dir[dir_len - 1] == '/') {
        dir_len--;
    }
    char *path = malloc(dir_len + strlen(entry) + 2);
    if (!path) {
        set_error(err_out, dup_cstr("out of memory"));
        return FAKELLM_TJS_ERROR;
    }
    memcpy(path, dir, dir_len);
    path[dir_len] = '/';
    strcpy(path + dir_len + 1, entry);

    JSValue argv[1] = { JS_NewString(h->ctx, path) };
    free(path);
    JSValue result = JS_UNDEFINED;
    int status = run(h, "load", 1, argv, timeout_ms, &result, err_out);
    JS_FreeValue(h->ctx, argv[0]);
    if (status != FAKELLM_TJS_OK) {
        return status;
    }

    JSValue error = JS_GetPropertyStr(h->ctx, result, "error");
    if (JS_IsString(error)) {
        set_error(err_out, dup_js_string(h->ctx, error));
        status = FAKELLM_TJS_ERROR;
    }
    JS_FreeValue(h->ctx, error);
    JS_FreeValue(h->ctx, result);
    return status;
}

/* Reads result.<name> as a malloc'd string, NULL when it is not a string. */
static char *string_property(JSContext *ctx, JSValueConst object, const char *name) {
    JSValue v = JS_GetPropertyStr(ctx, object, name);
    char *r = JS_IsString(v) ? dup_js_string(ctx, v) : NULL;
    JS_FreeValue(ctx, v);
    return r;
}

int fakellm_tjs_decide(fakellm_tjs *h, const char *request_json, const char *state_json, int64_t hits, int timeout_ms, char **reply_out,
                       char **state_out, char **err_out) {
    JSContext *ctx = h->ctx;
    *reply_out = NULL;
    *state_out = NULL;
    if (err_out) {
        *err_out = NULL;
    }

    JSValue argv[3] = { JS_NewString(ctx, request_json), JS_NewString(ctx, state_json), JS_NewInt64(ctx, hits) };
    JSValue result = JS_UNDEFINED;
    int status = run(h, "decide", 3, argv, timeout_ms, &result, err_out);
    JS_FreeValue(ctx, argv[0]);
    JS_FreeValue(ctx, argv[1]);
    JS_FreeValue(ctx, argv[2]);
    if (status != FAKELLM_TJS_OK) {
        return status;
    }

    char *error = string_property(ctx, result, "error");
    if (error) {
        set_error(err_out, error);
        status = FAKELLM_TJS_ERROR;
    } else {
        *state_out = string_property(ctx, result, "state");
        JSValue decline = JS_GetPropertyStr(ctx, result, "decline");
        if (JS_ToBool(ctx, decline) > 0) {
            status = FAKELLM_TJS_DECLINED;
        } else {
            *reply_out = string_property(ctx, result, "reply");
            if (!*reply_out) {
                set_error(err_out, dup_cstr("internal error: the policy wrapper returned no reply"));
                status = FAKELLM_TJS_ERROR;
            }
        }
        JS_FreeValue(ctx, decline);
    }
    JS_FreeValue(ctx, result);
    return status;
}

void fakellm_tjs_free(fakellm_tjs *h) {
    if (!h) {
        return;
    }
    JS_UpdateStackTop(h->rt);
    h->deadline_ns = 0;
    JS_FreeValue(h->ctx, h->api);
    uv_close((uv_handle_t *) &h->timer, NULL);
    uv_close((uv_handle_t *) &h->wake, NULL);
    TJS_FreeRuntime(h->qrt);
    free(h);
}

void fakellm_tjs_string_free(char *s) {
    free(s);
}
