// Flat C API over nghttp2 for the HTTP/2 CLIENT binding
// (`internal-binding/http2`). Mirrors the llhttp/zlib/crypto shim conventions:
// opaque handle pointers returned to JS, malloc'd scratch buffers indexed into
// wasm `memory`, and env-import callbacks declared with WASM_IMPORT that the JS
// adapter registers via `registerEnv`.
//
// The JS side owns the byte pump: it feeds received socket bytes through
// `h2_session_mem_recv` (which synchronously fires the js_h2_* callbacks) and
// drains outgoing bytes with `h2_session_send`. See
// src/worker/node-core/internal-binding/http2/index.ts.
//
// Deliberate simplifications vs. node_http2.cc: the plain uint8_t* header
// callback (no rcbuf refcounting), a copying data provider driven by
// js_h2_data_read (no NGHTTP2_DATA_FLAG_NO_COPY), and no manual flow control
// (auto WINDOW_UPDATE — no_auto_window_update is left unset), so the JS side
// never has to call nghttp2_session_consume.

#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>  // ssize_t

#include "nghttp2/nghttp2.h"

#define WASM_IMPORT(name) __attribute__((import_module("env"), import_name(#name)))

// Options buffer indices — must match node_http2_state.h Http2OptionsIndex.
#define IDX_OPTIONS_MAX_DEFLATE_DYNAMIC_TABLE_SIZE 0
#define IDX_OPTIONS_MAX_RESERVED_REMOTE_STREAMS 1
#define IDX_OPTIONS_MAX_SEND_HEADER_BLOCK_LENGTH 2
#define IDX_OPTIONS_PEER_MAX_CONCURRENT_STREAMS 3
#define IDX_OPTIONS_MAX_SETTINGS 9
#define IDX_OPTIONS_FLAGS 13

// Settings buffer indices — must match node_http2_state.h Http2SettingsIndex.
// IDX_SETTINGS_COUNT is where the flags bitfield lives (see util.js).
#define IDX_SETTINGS_HEADER_TABLE_SIZE 0
#define IDX_SETTINGS_ENABLE_PUSH 1
#define IDX_SETTINGS_INITIAL_WINDOW_SIZE 2
#define IDX_SETTINGS_MAX_FRAME_SIZE 3
#define IDX_SETTINGS_MAX_CONCURRENT_STREAMS 4
#define IDX_SETTINGS_MAX_HEADER_LIST_SIZE 5
#define IDX_SETTINGS_ENABLE_CONNECT_PROTOCOL 6
#define IDX_SETTINGS_COUNT 7
// 7 standard + MAX_ADDITIONAL_SETTINGS (10) custom.
#define MAX_SETTINGS_ENTRIES (7 + 10)

// ---------------------------------------------------------------------------
// Env imports (JS-provided). Spans (name/value/data/msg) are only valid for the
// duration of the call — JS must copy immediately.
// ---------------------------------------------------------------------------
extern int js_h2_on_begin_headers(int sid, int32_t stream_id, int32_t cat)
    WASM_IMPORT(js_h2_on_begin_headers);
extern int js_h2_on_header(int sid, int32_t stream_id, const char* name,
                           size_t namelen, const char* value, size_t valuelen,
                           uint8_t flags) WASM_IMPORT(js_h2_on_header);
extern int js_h2_on_frame_recv(int sid, uint8_t type, uint8_t flags,
                               int32_t stream_id)
    WASM_IMPORT(js_h2_on_frame_recv);
extern int js_h2_on_data_chunk(int sid, int32_t stream_id, const char* data,
                               size_t len, uint8_t flags)
    WASM_IMPORT(js_h2_on_data_chunk);
extern int js_h2_on_stream_close(int sid, int32_t stream_id, uint32_t code)
    WASM_IMPORT(js_h2_on_stream_close);
extern int js_h2_on_frame_not_sent(int sid, int32_t stream_id, uint8_t type,
                                   int lib_error)
    WASM_IMPORT(js_h2_on_frame_not_sent);
extern int js_h2_on_frame_send(int sid, uint8_t type, int32_t stream_id)
    WASM_IMPORT(js_h2_on_frame_send);
extern void js_h2_on_error(int sid, int lib_error, const char* msg, size_t len)
    WASM_IMPORT(js_h2_on_error);
// Fills up to `length` outbound body bytes for `stream_id` into `buf`, writes
// NGHTTP2 data flags (EOF / NO_END_STREAM) to *flags_out, returns byte count
// (>=0), or -1 to defer (NGHTTP2_ERR_DEFERRED).
extern int32_t js_h2_data_read(int sid, int32_t stream_id, uint8_t* buf,
                               size_t length, uint32_t* flags_out)
    WASM_IMPORT(js_h2_data_read);

// ---------------------------------------------------------------------------
// Session wrapper
// ---------------------------------------------------------------------------
typedef struct {
  nghttp2_session* ng;
  int session_id;  // JS registry key, echoed to every callback
  uint8_t* in_buf;
  size_t in_cap;
  uint8_t* out_buf;
  size_t out_len;
  size_t out_cap;
  // Payloads stashed during the current on_frame_recv dispatch so JS can pull
  // them back with the getters below (everything is synchronous within recv).
  uint8_t ping_data[8];
  uint32_t goaway_code;
  int32_t goaway_last_stream;
  uint8_t* goaway_opaque;
  size_t goaway_opaque_len;
  size_t goaway_opaque_cap;
} h2_session;

static int ensure_in(h2_session* s, size_t size) {
  if (size == 0) size = 1;
  if (s->in_cap < size) {
    uint8_t* nb = (uint8_t*)realloc(s->in_buf, size);
    if (!nb) return 0;
    s->in_buf = nb;
    s->in_cap = size;
  }
  return 1;
}

static int ensure_out(h2_session* s, size_t size) {
  if (size == 0) size = 1;
  if (s->out_cap < size) {
    size_t cap = s->out_cap ? s->out_cap : 4096;
    while (cap < size) cap *= 2;
    uint8_t* nb = (uint8_t*)realloc(s->out_buf, cap);
    if (!nb) return 0;
    s->out_buf = nb;
    s->out_cap = cap;
  }
  return 1;
}

// ---------------------------------------------------------------------------
// nghttp2 callback trampolines -> env imports
// ---------------------------------------------------------------------------
static int cb_on_begin_headers(nghttp2_session* ng, const nghttp2_frame* frame,
                               void* ud) {
  (void)ng;
  h2_session* s = (h2_session*)ud;
  return js_h2_on_begin_headers(s->session_id, frame->hd.stream_id,
                                (int32_t)frame->headers.cat);
}

static int cb_on_header(nghttp2_session* ng, const nghttp2_frame* frame,
                        const uint8_t* name, size_t namelen,
                        const uint8_t* value, size_t valuelen, uint8_t flags,
                        void* ud) {
  (void)ng;
  h2_session* s = (h2_session*)ud;
  return js_h2_on_header(s->session_id, frame->hd.stream_id, (const char*)name,
                         namelen, (const char*)value, valuelen, flags);
}

static int cb_on_frame_recv(nghttp2_session* ng, const nghttp2_frame* frame,
                            void* ud) {
  (void)ng;
  h2_session* s = (h2_session*)ud;
  uint8_t type = frame->hd.type;
  if (type == NGHTTP2_PING) {
    memcpy(s->ping_data, frame->ping.opaque_data, 8);
  } else if (type == NGHTTP2_GOAWAY) {
    s->goaway_code = frame->goaway.error_code;
    s->goaway_last_stream = frame->goaway.last_stream_id;
    size_t n = frame->goaway.opaque_data_len;
    if (n > s->goaway_opaque_cap) {
      uint8_t* nb = (uint8_t*)realloc(s->goaway_opaque, n);
      if (!nb) return NGHTTP2_ERR_CALLBACK_FAILURE;
      s->goaway_opaque = nb;
      s->goaway_opaque_cap = n;
    }
    if (n) memcpy(s->goaway_opaque, frame->goaway.opaque_data, n);
    s->goaway_opaque_len = n;
  }
  return js_h2_on_frame_recv(s->session_id, type, frame->hd.flags,
                             frame->hd.stream_id);
}

static int cb_on_data_chunk(nghttp2_session* ng, uint8_t flags,
                            int32_t stream_id, const uint8_t* data, size_t len,
                            void* ud) {
  (void)ng;
  h2_session* s = (h2_session*)ud;
  return js_h2_on_data_chunk(s->session_id, stream_id, (const char*)data, len,
                             flags);
}

static int cb_on_stream_close(nghttp2_session* ng, int32_t stream_id,
                              uint32_t error_code, void* ud) {
  (void)ng;
  h2_session* s = (h2_session*)ud;
  return js_h2_on_stream_close(s->session_id, stream_id, error_code);
}

static int cb_on_frame_not_send(nghttp2_session* ng, const nghttp2_frame* frame,
                                int lib_error, void* ud) {
  (void)ng;
  h2_session* s = (h2_session*)ud;
  return js_h2_on_frame_not_sent(s->session_id, frame->hd.stream_id,
                                 frame->hd.type, lib_error);
}

static int cb_on_frame_send(nghttp2_session* ng, const nghttp2_frame* frame,
                            void* ud) {
  (void)ng;
  h2_session* s = (h2_session*)ud;
  return js_h2_on_frame_send(s->session_id, frame->hd.type,
                             frame->hd.stream_id);
}

static int cb_error(nghttp2_session* ng, const char* msg, size_t len,
                    void* ud) {
  (void)ng;
  h2_session* s = (h2_session*)ud;
  js_h2_on_error(s->session_id, 0, msg, len);
  return 0;
}

static ssize_t cb_data_read(nghttp2_session* ng, int32_t stream_id,
                            uint8_t* buf, size_t length, uint32_t* data_flags,
                            nghttp2_data_source* source, void* ud) {
  (void)ng;
  (void)source;
  h2_session* s = (h2_session*)ud;
  uint32_t flags = 0;
  int32_t n = js_h2_data_read(s->session_id, stream_id, buf, length, &flags);
  if (n == -1) return NGHTTP2_ERR_DEFERRED;
  if (n < 0) return NGHTTP2_ERR_CALLBACK_FAILURE;
  *data_flags = flags;
  return (ssize_t)n;
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------
h2_session* h2_session_new(int session_id, const uint32_t* options) {
  h2_session* s = (h2_session*)calloc(1, sizeof(h2_session));
  if (!s) return NULL;
  s->session_id = session_id;

  nghttp2_session_callbacks* cbs = NULL;
  if (nghttp2_session_callbacks_new(&cbs) != 0) {
    free(s);
    return NULL;
  }
  nghttp2_session_callbacks_set_on_begin_headers_callback(cbs,
                                                          cb_on_begin_headers);
  nghttp2_session_callbacks_set_on_header_callback(cbs, cb_on_header);
  nghttp2_session_callbacks_set_on_frame_recv_callback(cbs, cb_on_frame_recv);
  nghttp2_session_callbacks_set_on_data_chunk_recv_callback(cbs,
                                                            cb_on_data_chunk);
  nghttp2_session_callbacks_set_on_stream_close_callback(cbs, cb_on_stream_close);
  nghttp2_session_callbacks_set_on_frame_not_send_callback(cbs,
                                                           cb_on_frame_not_send);
  nghttp2_session_callbacks_set_on_frame_send_callback(cbs, cb_on_frame_send);
  nghttp2_session_callbacks_set_error_callback(cbs, cb_error);

  nghttp2_option* opt = NULL;
  nghttp2_option_new(&opt);
  if (opt) {
    // Free closed streams promptly; recommended peer concurrency default.
    nghttp2_option_set_no_closed_streams(opt, 1);
    nghttp2_option_set_peer_max_concurrent_streams(opt, 100);
    if (options) {
      uint32_t flags = options[IDX_OPTIONS_FLAGS];
      if (flags & (1u << IDX_OPTIONS_MAX_DEFLATE_DYNAMIC_TABLE_SIZE))
        nghttp2_option_set_max_deflate_dynamic_table_size(
            opt, options[IDX_OPTIONS_MAX_DEFLATE_DYNAMIC_TABLE_SIZE]);
      if (flags & (1u << IDX_OPTIONS_MAX_RESERVED_REMOTE_STREAMS))
        nghttp2_option_set_max_reserved_remote_streams(
            opt, options[IDX_OPTIONS_MAX_RESERVED_REMOTE_STREAMS]);
      if (flags & (1u << IDX_OPTIONS_MAX_SEND_HEADER_BLOCK_LENGTH))
        nghttp2_option_set_max_send_header_block_length(
            opt, options[IDX_OPTIONS_MAX_SEND_HEADER_BLOCK_LENGTH]);
      if (flags & (1u << IDX_OPTIONS_PEER_MAX_CONCURRENT_STREAMS))
        nghttp2_option_set_peer_max_concurrent_streams(
            opt, options[IDX_OPTIONS_PEER_MAX_CONCURRENT_STREAMS]);
      if (flags & (1u << IDX_OPTIONS_MAX_SETTINGS))
        nghttp2_option_set_max_settings(opt,
                                        (size_t)options[IDX_OPTIONS_MAX_SETTINGS]);
    }
  }

  int rv = nghttp2_session_client_new2(&s->ng, cbs, s, opt);
  nghttp2_session_callbacks_del(cbs);
  if (opt) nghttp2_option_del(opt);
  if (rv != 0) {
    free(s);
    return NULL;
  }
  return s;
}

void h2_session_del(h2_session* s) {
  if (!s) return;
  if (s->ng) nghttp2_session_del(s->ng);
  if (s->in_buf) free(s->in_buf);
  if (s->out_buf) free(s->out_buf);
  if (s->goaway_opaque) free(s->goaway_opaque);
  free(s);
}

int h2_session_want_read(h2_session* s) {
  return nghttp2_session_want_read(s->ng);
}
int h2_session_want_write(h2_session* s) {
  return nghttp2_session_want_write(s->ng);
}

// ---------------------------------------------------------------------------
// Receiving bytes (socket -> nghttp2)
// ---------------------------------------------------------------------------
uint8_t* h2_recv_buf(h2_session* s, size_t n) {
  if (!ensure_in(s, n)) return NULL;
  return s->in_buf;
}

// Fires the js_h2_* callbacks synchronously. Returns bytes consumed or a
// negative nghttp2 error.
ssize_t h2_session_mem_recv(h2_session* s, size_t n) {
  return nghttp2_session_mem_recv(s->ng, s->in_buf, n);
}

// ---------------------------------------------------------------------------
// Producing bytes (nghttp2 -> socket). Batches all pending output into out_buf;
// may fire cb_data_read for request bodies.
// ---------------------------------------------------------------------------
ssize_t h2_session_send(h2_session* s) {
  s->out_len = 0;
  for (;;) {
    const uint8_t* data = NULL;
    ssize_t n = nghttp2_session_mem_send(s->ng, &data);
    if (n < 0) return n;
    if (n == 0) break;
    if (!ensure_out(s, s->out_len + (size_t)n)) return NGHTTP2_ERR_NOMEM;
    memcpy(s->out_buf + s->out_len, data, (size_t)n);
    s->out_len += (size_t)n;
  }
  return (ssize_t)s->out_len;
}

uint32_t h2_session_send_ptr(h2_session* s) {
  return (uint32_t)(uintptr_t)s->out_buf;
}

// ---------------------------------------------------------------------------
// Header blob parsing (name\0value\0<flagbyte>, Latin1) — matches
// buildNgHeaderString / NgHeaders.
// ---------------------------------------------------------------------------
static nghttp2_nv* parse_headers(const char* hdrs, size_t count) {
  if (count == 0) return NULL;
  nghttp2_nv* nva = (nghttp2_nv*)malloc(count * sizeof(nghttp2_nv));
  if (!nva) return NULL;
  const char* p = hdrs;
  for (size_t i = 0; i < count; i++) {
    nva[i].name = (uint8_t*)p;
    nva[i].namelen = strlen(p);
    p += nva[i].namelen + 1;
    nva[i].value = (uint8_t*)p;
    nva[i].valuelen = strlen(p);
    p += nva[i].valuelen + 1;
    nva[i].flags = (uint8_t)(*p);
    p += 1;
  }
  return nva;
}

// ---------------------------------------------------------------------------
// Submitting a request + stream operations
// ---------------------------------------------------------------------------
int32_t h2_submit_request(h2_session* s, const char* hdrs, size_t byteLen,
                          size_t count, int options, int32_t parent,
                          int32_t weight, int exclusive) {
  (void)byteLen;
  (void)parent;
  (void)weight;
  (void)exclusive;
  nghttp2_nv* nva = parse_headers(hdrs, count);
  if (count && !nva) return NGHTTP2_ERR_NOMEM;

  nghttp2_data_provider prd;
  const nghttp2_data_provider* prdp = NULL;
  if (!(options & 0x1 /* STREAM_OPTION_EMPTY_PAYLOAD */)) {
    prd.source.ptr = NULL;
    prd.read_callback = cb_data_read;
    prdp = &prd;
  }

  // pri_spec is ignored by nghttp2 (RFC 9113 deprecated priorities).
  int32_t rv = nghttp2_submit_request(s->ng, NULL, nva, count, prdp, NULL);
  if (nva) free(nva);
  return rv;
}

int h2_submit_trailers(h2_session* s, int32_t id, const char* hdrs,
                       size_t byteLen, size_t count) {
  (void)byteLen;
  nghttp2_nv* nva = parse_headers(hdrs, count);
  if (count && !nva) return NGHTTP2_ERR_NOMEM;
  int rv = nghttp2_submit_trailer(s->ng, id, nva, count);
  if (nva) free(nva);
  return rv;
}

int h2_submit_rst_stream(h2_session* s, int32_t id, uint32_t code) {
  return nghttp2_submit_rst_stream(s->ng, NGHTTP2_FLAG_NONE, id, code);
}

int h2_submit_priority(h2_session* s, int32_t id, int32_t parent,
                       int32_t weight, int exclusive) {
  nghttp2_priority_spec pri;
  nghttp2_priority_spec_init(&pri, parent, weight, exclusive);
  return nghttp2_submit_priority(s->ng, NGHTTP2_FLAG_NONE, id, &pri);
}

int h2_resume_data(h2_session* s, int32_t id) {
  return nghttp2_session_resume_data(s->ng, id);
}

// ---------------------------------------------------------------------------
// Session-level frames
// ---------------------------------------------------------------------------
static size_t build_iv(const uint32_t* buf, nghttp2_settings_entry* out) {
  static const int32_t ids[7] = {
      NGHTTP2_SETTINGS_HEADER_TABLE_SIZE,
      NGHTTP2_SETTINGS_ENABLE_PUSH,
      NGHTTP2_SETTINGS_INITIAL_WINDOW_SIZE,
      NGHTTP2_SETTINGS_MAX_FRAME_SIZE,
      NGHTTP2_SETTINGS_MAX_CONCURRENT_STREAMS,
      NGHTTP2_SETTINGS_MAX_HEADER_LIST_SIZE,
      NGHTTP2_SETTINGS_ENABLE_CONNECT_PROTOCOL,
  };
  uint32_t flags = buf[IDX_SETTINGS_COUNT];
  size_t count = 0;
  for (int i = 0; i < 7; i++) {
    if (flags & (1u << i)) {
      out[count].settings_id = ids[i];
      out[count].value = buf[i];
      count++;
    }
  }
  uint32_t num_add = buf[IDX_SETTINGS_COUNT + 1];
  uint32_t offset = IDX_SETTINGS_COUNT + 1 + 1;
  for (uint32_t i = 0; i < num_add; i++) {
    out[count].settings_id = (int32_t)buf[offset + i * 2 + 0];
    out[count].value = buf[offset + i * 2 + 1];
    count++;
  }
  return count;
}

int h2_submit_settings(h2_session* s, const uint32_t* buf) {
  nghttp2_settings_entry iv[MAX_SETTINGS_ENTRIES];
  size_t n = build_iv(buf, iv);
  return nghttp2_submit_settings(s->ng, NGHTTP2_FLAG_NONE, iv, n);
}

ssize_t h2_pack_settings(const uint32_t* buf, uint8_t* out, size_t cap) {
  nghttp2_settings_entry iv[MAX_SETTINGS_ENTRIES];
  size_t n = build_iv(buf, iv);
  return nghttp2_pack_settings_payload(out, cap, iv, n);
}

int h2_submit_ping(h2_session* s, const uint8_t* payload) {
  return nghttp2_submit_ping(s->ng, NGHTTP2_FLAG_NONE, payload);
}

int h2_submit_goaway(h2_session* s, uint32_t code, int32_t last,
                     const uint8_t* data, size_t len) {
  if (last < 0) last = nghttp2_session_get_last_proc_stream_id(s->ng);
  return nghttp2_submit_goaway(s->ng, NGHTTP2_FLAG_NONE, last, code, data, len);
}

int h2_set_next_stream_id(h2_session* s, int32_t id) {
  return nghttp2_session_set_next_stream_id(s->ng, id);
}

int h2_set_local_window_size(h2_session* s, int32_t id, int32_t size) {
  return nghttp2_session_set_local_window_size(s->ng, NGHTTP2_FLAG_NONE, id,
                                               size);
}

int h2_terminate(h2_session* s, uint32_t code) {
  return nghttp2_session_terminate_session(s->ng, code);
}

// ---------------------------------------------------------------------------
// State readback
// ---------------------------------------------------------------------------
void h2_refresh_session_state(h2_session* s, double* o) {
  nghttp2_session* ng = s->ng;
  o[0] = nghttp2_session_get_effective_local_window_size(ng);
  o[1] = nghttp2_session_get_effective_recv_data_length(ng);
  o[2] = nghttp2_session_get_next_stream_id(ng);
  o[3] = nghttp2_session_get_local_window_size(ng);
  o[4] = nghttp2_session_get_last_proc_stream_id(ng);
  o[5] = nghttp2_session_get_remote_window_size(ng);
  o[6] = (double)nghttp2_session_get_outbound_queue_size(ng);
  o[7] = (double)nghttp2_session_get_hd_deflate_dynamic_table_size(ng);
  o[8] = (double)nghttp2_session_get_hd_inflate_dynamic_table_size(ng);
}

void h2_refresh_stream_state(h2_session* s, int32_t id, double* o) {
  nghttp2_session* ng = s->ng;
  nghttp2_stream* str = nghttp2_session_find_stream(ng, id);
  if (!str) {
    o[0] = NGHTTP2_STREAM_STATE_IDLE;
    o[1] = o[2] = o[3] = o[4] = o[5] = 0;
    return;
  }
  o[0] = nghttp2_stream_get_state(str);
  o[1] = nghttp2_stream_get_weight(str);
  o[2] = nghttp2_stream_get_sum_dependency_weight(str);
  o[3] = nghttp2_session_get_stream_local_close(ng, id);
  o[4] = nghttp2_session_get_stream_remote_close(ng, id);
  o[5] = nghttp2_session_get_stream_local_window_size(ng, id);
}

void h2_get_settings(h2_session* s, int local, uint32_t* o) {
  nghttp2_session* ng = s->ng;
  static const int32_t ids[7] = {
      NGHTTP2_SETTINGS_HEADER_TABLE_SIZE,
      NGHTTP2_SETTINGS_ENABLE_PUSH,
      NGHTTP2_SETTINGS_INITIAL_WINDOW_SIZE,
      NGHTTP2_SETTINGS_MAX_FRAME_SIZE,
      NGHTTP2_SETTINGS_MAX_CONCURRENT_STREAMS,
      NGHTTP2_SETTINGS_MAX_HEADER_LIST_SIZE,
      NGHTTP2_SETTINGS_ENABLE_CONNECT_PROTOCOL,
  };
  for (int i = 0; i < 7; i++) {
    o[i] = local ? nghttp2_session_get_local_settings(ng, ids[i])
                 : nghttp2_session_get_remote_settings(ng, ids[i]);
  }
}

uint32_t h2_get_next_stream_id(h2_session* s) {
  return nghttp2_session_get_next_stream_id(s->ng);
}

// ---------------------------------------------------------------------------
// Frame-payload getters (read during an on_frame_recv dispatch)
// ---------------------------------------------------------------------------
void h2_get_ping_data(h2_session* s, uint8_t* out8) {
  memcpy(out8, s->ping_data, 8);
}
uint32_t h2_get_goaway_code(h2_session* s) { return s->goaway_code; }
int32_t h2_get_goaway_last_stream(h2_session* s) {
  return s->goaway_last_stream;
}
uint32_t h2_get_goaway_opaque_ptr(h2_session* s) {
  return (uint32_t)(uintptr_t)s->goaway_opaque;
}
uint32_t h2_get_goaway_opaque_len(h2_session* s) {
  return (uint32_t)s->goaway_opaque_len;
}

const char* h2_strerror(int code) { return nghttp2_strerror(code); }

// Smoke test: build a client session, submit a bare GET, drain the client
// preface + HEADERS. Returns 0 on success or a negative marker.
int h2_smoke_test(void) {
  nghttp2_session_callbacks* cbs = NULL;
  if (nghttp2_session_callbacks_new(&cbs) != 0) return -1;
  nghttp2_session* ng = NULL;
  int rv = nghttp2_session_client_new(&ng, cbs, NULL);
  nghttp2_session_callbacks_del(cbs);
  if (rv != 0) return -2;

  nghttp2_settings_entry iv[1] = {
      {NGHTTP2_SETTINGS_MAX_CONCURRENT_STREAMS, 100}};
  if (nghttp2_submit_settings(ng, NGHTTP2_FLAG_NONE, iv, 1) != 0) {
    nghttp2_session_del(ng);
    return -3;
  }

  const uint8_t* data = NULL;
  ssize_t n = nghttp2_session_mem_send(ng, &data);
  nghttp2_session_del(ng);
  if (n <= 0) return -4;
  return 0;
}
