#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>

#include "llhttp.h"

#define LLHTTP_WASM_SMOKE_REQUEST \
  "GET /smoke HTTP/1.1\r\n" \
  "Host: example.com\r\n" \
  "Connection: keep-alive\r\n" \
  "\r\n"

#define WASM_IMPORT(name) __attribute__((import_module("env"), import_name(#name)))

extern int wasm_on_message_begin(llhttp_t* parser) WASM_IMPORT(wasm_on_message_begin);
extern int wasm_on_url(llhttp_t* parser,
                       const char* at,
                       size_t length) WASM_IMPORT(wasm_on_url);
extern int wasm_on_status(llhttp_t* parser,
                          const char* at,
                          size_t length) WASM_IMPORT(wasm_on_status);
extern int wasm_on_header_field(llhttp_t* parser,
                                const char* at,
                                size_t length) WASM_IMPORT(wasm_on_header_field);
extern int wasm_on_header_value(llhttp_t* parser,
                                const char* at,
                                size_t length) WASM_IMPORT(wasm_on_header_value);
extern int wasm_on_headers_complete(llhttp_t* parser,
                                    int status_code,
                                    uint8_t upgrade,
                                    int should_keep_alive) WASM_IMPORT(wasm_on_headers_complete);
extern int wasm_on_body(llhttp_t* parser,
                        const char* at,
                        size_t length) WASM_IMPORT(wasm_on_body);
extern int wasm_on_message_complete(llhttp_t* parser) WASM_IMPORT(wasm_on_message_complete);

static llhttp_settings_t settings;
static int settings_initialized = 0;

static int wasm_on_headers_complete_wrap(llhttp_t* parser) {
  return wasm_on_headers_complete(parser,
                                  parser->status_code,
                                  parser->upgrade,
                                  llhttp_should_keep_alive(parser));
}

static void ensure_settings(void) {
  if (settings_initialized) {
    return;
  }

  llhttp_settings_init(&settings);
  settings.on_message_begin = wasm_on_message_begin;
  settings.on_url = wasm_on_url;
  settings.on_status = wasm_on_status;
  settings.on_header_field = wasm_on_header_field;
  settings.on_header_value = wasm_on_header_value;
  settings.on_headers_complete = wasm_on_headers_complete_wrap;
  settings.on_body = wasm_on_body;
  settings.on_message_complete = wasm_on_message_complete;
  settings_initialized = 1;
}

llhttp_t* llhttp_wasm_alloc(llhttp_type_t type) {
  llhttp_t* parser;

  ensure_settings();
  parser = malloc(sizeof(llhttp_t));
  if (parser == NULL) {
    return NULL;
  }

  llhttp_init(parser, type, &settings);
  return parser;
}

void llhttp_wasm_free(llhttp_t* parser) {
  free(parser);
}

void llhttp_wasm_init(llhttp_t* parser, llhttp_type_t type) {
  ensure_settings();
  llhttp_init(parser, type, &settings);
}

int llhttp_wasm_smoke_test(void) {
  llhttp_t parser;
  llhttp_errno_t err;
  static const char request[] = LLHTTP_WASM_SMOKE_REQUEST;

  llhttp_wasm_init(&parser, HTTP_REQUEST);

  err = llhttp_execute(&parser, request, sizeof(request) - 1);
  if (err != HPE_OK) {
    return (int) err;
  }

  if (llhttp_get_type(&parser) != HTTP_REQUEST) {
    return -1;
  }
  if (llhttp_get_http_major(&parser) != 1 || llhttp_get_http_minor(&parser) != 1) {
    return -2;
  }
  if (llhttp_get_method(&parser) != HTTP_GET) {
    return -3;
  }
  if (!llhttp_should_keep_alive(&parser)) {
    return -4;
  }

  err = llhttp_finish(&parser);
  if (err != HPE_OK) {
    return (int) err;
  }

  return 0;
}
