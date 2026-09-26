#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include "zlib.h"

// node_zlib_mode values (must match constants.ts + upstream node_zlib.cc).
#define MODE_NONE       0
#define MODE_DEFLATE    1
#define MODE_INFLATE    2
#define MODE_GZIP       3
#define MODE_GUNZIP     4
#define MODE_DEFLATERAW 5
#define MODE_INFLATERAW 6
#define MODE_UNZIP      7

typedef struct {
  z_stream strm;
  int mode;
  int err;
  int initialized;
  uint8_t* in_buf;
  size_t in_cap;
  uint8_t* out_buf;
  size_t out_cap;
  uint8_t* dict;
  size_t dict_len;
  int dict_set_on_inflate;
} zlib_handle_t;

static int is_deflate_mode(int mode) {
  return mode == MODE_DEFLATE || mode == MODE_GZIP || mode == MODE_DEFLATERAW;
}

static int is_inflate_mode(int mode) {
  return mode == MODE_INFLATE || mode == MODE_GUNZIP ||
         mode == MODE_INFLATERAW || mode == MODE_UNZIP;
}

zlib_handle_t* zlib_alloc(int mode) {
  if (mode < MODE_DEFLATE || mode > MODE_UNZIP) {
    return NULL;
  }
  zlib_handle_t* h = (zlib_handle_t*)calloc(1, sizeof(zlib_handle_t));
  if (!h) {
    return NULL;
  }
  h->mode = mode;
  h->err = Z_OK;
  return h;
}

int zlib_init(zlib_handle_t* h,
              int windowBits,
              int level,
              int memLevel,
              int strategy,
              const uint8_t* dict,
              size_t dict_len) {
  if (!h) return Z_STREAM_ERROR;

  int wb = windowBits;
  if (h->mode == MODE_GZIP || h->mode == MODE_GUNZIP) {
    wb += 16;
  } else if (h->mode == MODE_UNZIP) {
    // Auto-detect zlib vs gzip wrapper.
    wb += 32;
  } else if (h->mode == MODE_DEFLATERAW || h->mode == MODE_INFLATERAW) {
    wb = -wb;
  }

  if (is_deflate_mode(h->mode)) {
    h->err = deflateInit2(&h->strm, level, Z_DEFLATED, wb, memLevel, strategy);
    if (h->err == Z_OK && dict && dict_len) {
      h->dict = (uint8_t*)malloc(dict_len);
      if (!h->dict) {
        return Z_MEM_ERROR;
      }
      memcpy(h->dict, dict, dict_len);
      h->dict_len = dict_len;
      h->err = deflateSetDictionary(&h->strm, h->dict, (uInt)h->dict_len);
    }
  } else if (is_inflate_mode(h->mode)) {
    h->err = inflateInit2(&h->strm, wb);
    if (h->err == Z_OK && dict && dict_len) {
      // Inflate dictionaries are applied lazily on Z_NEED_DICT.
      h->dict = (uint8_t*)malloc(dict_len);
      if (!h->dict) {
        return Z_MEM_ERROR;
      }
      memcpy(h->dict, dict, dict_len);
      h->dict_len = dict_len;
    }
  } else {
    return Z_STREAM_ERROR;
  }

  h->initialized = (h->err == Z_OK || h->err == Z_STREAM_ERROR) ? 1 : 1;
  return h->err;
}

uint8_t* zlib_ensure_in_buf(zlib_handle_t* h, size_t size) {
  if (!h) return NULL;
  if (size == 0) size = 1;
  if (h->in_cap < size) {
    uint8_t* nb = (uint8_t*)realloc(h->in_buf, size);
    if (!nb) return NULL;
    h->in_buf = nb;
    h->in_cap = size;
  }
  return h->in_buf;
}

uint8_t* zlib_ensure_out_buf(zlib_handle_t* h, size_t size) {
  if (!h) return NULL;
  if (size == 0) size = 1;
  if (h->out_cap < size) {
    uint8_t* nb = (uint8_t*)realloc(h->out_buf, size);
    if (!nb) return NULL;
    h->out_buf = nb;
    h->out_cap = size;
  }
  return h->out_buf;
}

int zlib_write(zlib_handle_t* h, int flush, size_t in_len, size_t out_len) {
  if (!h || !h->initialized) return Z_STREAM_ERROR;

  h->strm.next_in = h->in_buf;
  h->strm.avail_in = (uInt)in_len;
  h->strm.next_out = h->out_buf;
  h->strm.avail_out = (uInt)out_len;

  if (is_deflate_mode(h->mode)) {
    h->err = deflate(&h->strm, flush);
  } else if (is_inflate_mode(h->mode)) {
    h->err = inflate(&h->strm, flush);

    if (h->err == Z_NEED_DICT && h->dict) {
      h->err = inflateSetDictionary(&h->strm, h->dict, (uInt)h->dict_len);
      if (h->err == Z_OK) {
        h->err = inflate(&h->strm, flush);
      } else if (h->err == Z_DATA_ERROR) {
        // Both inflateSetDictionary() and inflate() can return Z_DATA_ERROR;
        // surface the dictionary problem distinctly.
        h->err = Z_NEED_DICT;
      }
    }

    // Trailing concatenated gzip members. Matches the loop the upstream
    // pako-based shim used to run, but does it inline so JS doesn't have to
    // poke at the stream state.
    while (h->strm.avail_in > 0 &&
           h->mode == MODE_GUNZIP &&
           h->err == Z_STREAM_END &&
           h->strm.next_in[0] != 0x00) {
      inflateReset(&h->strm);
      h->err = inflate(&h->strm, flush);
    }
  } else {
    return Z_STREAM_ERROR;
  }

  return h->err;
}

uint32_t zlib_avail_in(zlib_handle_t* h) {
  return h ? (uint32_t)h->strm.avail_in : 0;
}

uint32_t zlib_avail_out(zlib_handle_t* h) {
  return h ? (uint32_t)h->strm.avail_out : 0;
}

int zlib_get_err(zlib_handle_t* h) {
  return h ? h->err : Z_STREAM_ERROR;
}

const char* zlib_get_msg(zlib_handle_t* h) {
  if (!h) return NULL;
  return h->strm.msg;
}

int zlib_params(zlib_handle_t* h, int level, int strategy) {
  if (!h || !h->initialized) return Z_STREAM_ERROR;
  if (!is_deflate_mode(h->mode)) {
    h->err = Z_OK;
    return Z_OK;
  }
  // Best-effort: deflateParams may flush pending output. With no output
  // buffer bound here it returns Z_BUF_ERROR, which we swallow — the next
  // zlib_write() will emit the deferred bytes once a real out buffer exists.
  int err = deflateParams(&h->strm, level, strategy);
  if (err == Z_BUF_ERROR) err = Z_OK;
  h->err = err;
  return err;
}

int zlib_reset(zlib_handle_t* h) {
  if (!h || !h->initialized) return Z_STREAM_ERROR;
  if (is_deflate_mode(h->mode)) {
    h->err = deflateReset(&h->strm);
    if (h->err == Z_OK && h->dict) {
      h->err = deflateSetDictionary(&h->strm, h->dict, (uInt)h->dict_len);
    }
  } else if (is_inflate_mode(h->mode)) {
    h->err = inflateReset(&h->strm);
  } else {
    return Z_STREAM_ERROR;
  }
  return h->err;
}

void zlib_end(zlib_handle_t* h) {
  if (!h) return;
  if (h->initialized) {
    if (is_deflate_mode(h->mode)) {
      deflateEnd(&h->strm);
    } else if (is_inflate_mode(h->mode)) {
      inflateEnd(&h->strm);
    }
  }
  if (h->in_buf) free(h->in_buf);
  if (h->out_buf) free(h->out_buf);
  if (h->dict) free(h->dict);
  free(h);
}

uint32_t zlib_crc32_buf(const uint8_t* ptr, size_t len, uint32_t initial) {
  return (uint32_t)crc32((uLong)initial, ptr, (uInt)len);
}

int zlib_smoke_test(void) {
  z_stream s;
  memset(&s, 0, sizeof(s));

  static const uint8_t input[] = "node-worker zlib smoke";
  uint8_t compressed[128];
  uint8_t roundtrip[128];

  if (deflateInit(&s, Z_DEFAULT_COMPRESSION) != Z_OK) return -1;
  s.next_in = (z_const Bytef*)input;
  s.avail_in = (uInt)(sizeof(input) - 1);
  s.next_out = compressed;
  s.avail_out = sizeof(compressed);
  int err = deflate(&s, Z_FINISH);
  size_t produced = sizeof(compressed) - s.avail_out;
  deflateEnd(&s);
  if (err != Z_STREAM_END) return -2;

  memset(&s, 0, sizeof(s));
  if (inflateInit(&s) != Z_OK) return -3;
  s.next_in = compressed;
  s.avail_in = (uInt)produced;
  s.next_out = roundtrip;
  s.avail_out = sizeof(roundtrip);
  err = inflate(&s, Z_FINISH);
  size_t got = sizeof(roundtrip) - s.avail_out;
  inflateEnd(&s);
  if (err != Z_STREAM_END) return -4;
  if (got != sizeof(input) - 1) return -5;
  if (memcmp(roundtrip, input, got) != 0) return -6;
  return 0;
}
