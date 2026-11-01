/*
 * fakellm <-> txiki.js glue.
 *
 * A small, stable C API over the embedded txiki.js runtime (QuickJS + libuv). One handle is one JavaScript runtime holding one policy module.
 *
 * Threading: a handle is not thread safe. It can be moved between threads, but only one thread may use it at a time. Every call must run
 * on a thread whose stack is larger than the stack_size given to fakellm_tjs_new plus some headroom for the C frames.
 *
 * Strings returned through char ** out parameters are malloc'd and must be released with fakellm_tjs_string_free.
 */
#ifndef FAKELLM_TJS_H
#define FAKELLM_TJS_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct fakellm_tjs fakellm_tjs;

/* Status codes returned by fakellm_tjs_load and fakellm_tjs_decide. */
#define FAKELLM_TJS_OK 0
#define FAKELLM_TJS_DECLINED 1 /* decide only: the policy returned null, undefined or {decline: true} */
#define FAKELLM_TJS_ERROR 2    /* err_out holds the message */
#define FAKELLM_TJS_TIMEOUT 3  /* the deadline passed; the handle is unusable afterwards, free it and create a new one */
#define FAKELLM_TJS_CANCELLED 4 /* fakellm_tjs_cancel was called; the handle is unusable, free it */

/* Receives every log line (ctx.log and console.*), without the trailing newline. Called on the thread that is inside a fakellm_tjs_* call. */
typedef void (*fakellm_tjs_log_fn)(void *user, const char *line, size_t len);

/* Creates a runtime. mem_limit is in bytes (0 = unlimited), stack_size is the JS stack limit in bytes (0 = txiki.js default). NULL on failure. */
fakellm_tjs *fakellm_tjs_new(int mem_limit, size_t stack_size);

/* Sets the log sink. Without one, log lines go to stderr. */
void fakellm_tjs_set_log(fakellm_tjs *h, fakellm_tjs_log_fn fn, void *user);

/*
 * Loads the ES module dir/entry (dir must be absolute) and keeps its default export as the policy. Top-level await is supported; the event
 * loop runs until the module has finished evaluating. timeout_ms <= 0 means no deadline. Call once per handle.
 * Returns FAKELLM_TJS_OK, FAKELLM_TJS_ERROR (syntax error, missing file, throw at top level, no default function export) or FAKELLM_TJS_TIMEOUT.
 */
int fakellm_tjs_load(fakellm_tjs *h, const char *dir, const char *entry, int timeout_ms, char **err_out);

/*
 * Calls decide(request, {state, hits, log}) with JSON.parse'd request_json and state_json and drives the event loop until the returned
 * promise settles. timeout_ms <= 0 means no deadline; it covers the whole call, including awaits.
 *
 * OK:       *reply_out is the JSON of the reply object without its `state` field; *state_out is the JSON of the returned `state` or NULL if
 *           the policy returned none (keep the old one).
 * DECLINED: *reply_out is NULL; *state_out as above.
 * ERROR:    *err_out is the message (thrown value, invalid reply, an unhandled rejection, a promise that can never settle).
 * TIMEOUT:  the deadline passed (a runaway loop is interrupted, a pending await is abandoned).
 * Outputs not written are set to NULL.
 */
int fakellm_tjs_decide(fakellm_tjs *h, const char *request_json, const char *state_json, int64_t hits, int timeout_ms, char **reply_out,
                       char **state_out, char **err_out);

/*
 * Cancels the running call, and every later one, from any thread: the call returns FAKELLM_TJS_CANCELLED as soon as it reaches an interrupt
 * point or the event loop (a runaway loop and a pending await both end). Sticky; the handle can only be freed afterwards. The caller must
 * guarantee that the handle is not freed while this runs.
 */
void fakellm_tjs_cancel(fakellm_tjs *h);

/* Destroys the runtime (closes timers and handles the policy left open). NULL is fine. */
void fakellm_tjs_free(fakellm_tjs *h);

/* Releases a string returned through an out parameter. NULL is fine. */
void fakellm_tjs_string_free(char *s);

#ifdef __cplusplus
}
#endif

#endif
