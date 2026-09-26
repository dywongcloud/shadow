#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include "brotli/encode.h"
#include "brotli/decode.h"

// node_zlib_mode values (must match constants.ts).
#define MODE_BROTLI_DECODE 8
#define MODE_BROTLI_ENCODE 9

// `err` semantics mirror the zlib shim:
//   1  = stream end / finished
//   0  = ok, more work pending
//  -1  = error (consult brotli_get_msg for decoder)
#define BR_OK         0
#define BR_STREAM_END 1
#define BR_ERROR     -1

typedef struct {
  int is_encoder;
  union {
    BrotliEncoderState* enc;
    BrotliDecoderState* dec;
  } s;
  int err;
  uint8_t* in_buf;
  size_t in_cap;
  uint8_t* out_buf;
  size_t out_cap;
  uint8_t* dict;
  size_t dict_len;
  // Updated by brotli_write so JS can read them back without struct layout
  // assumptions.
  size_t avail_in_after;
  size_t avail_out_after;
} brotli_handle_t;

brotli_handle_t* brotli_alloc(int mode) {
  brotli_handle_t* h = (brotli_handle_t*)calloc(1, sizeof(brotli_handle_t));
  if (!h) return NULL;

  if (mode == MODE_BROTLI_ENCODE) {
    h->is_encoder = 1;
    h->s.enc = BrotliEncoderCreateInstance(NULL, NULL, NULL);
    if (!h->s.enc) {
      free(h);
      return NULL;
    }
  } else if (mode == MODE_BROTLI_DECODE) {
    h->is_encoder = 0;
    h->s.dec = BrotliDecoderCreateInstance(NULL, NULL, NULL);
    if (!h->s.dec) {
      free(h);
      return NULL;
    }
  } else {
    free(h);
    return NULL;
  }

  h->err = BR_OK;
  return h;
}

// Apply parameter overrides from the per-stream init array. Each slot is
// either -1 (unset, skip) or the desired value. `params_len` is the number of
// slots, which the JS side derives from `kMaxBrotliParam + 1` so we don't have
// to keep the constants in sync here.
static int apply_params(brotli_handle_t* h, const int32_t* params, size_t params_len) {
  if (!params) return 0;
  for (size_t i = 0; i < params_len; ++i) {
    int32_t v = params[i];
    if (v == -1) continue;

    if (h->is_encoder) {
      if (!BrotliEncoderSetParameter(
              h->s.enc,
              (BrotliEncoderParameter)i,
              (uint32_t)v)) {
        return -1;
      }
    } else {
      // Decoder only accepts a small subset of parameter slots; anything else
      // is silently ignored so a shared param array doesn't fail decode.
      if (i != BROTLI_DECODER_PARAM_DISABLE_RING_BUFFER_REALLOCATION &&
          i != BROTLI_DECODER_PARAM_LARGE_WINDOW) {
        continue;
      }
      if (!BrotliDecoderSetParameter(
              h->s.dec,
              (BrotliDecoderParameter)i,
              (uint32_t)v)) {
        return -1;
      }
    }
  }
  return 0;
}

int brotli_init(brotli_handle_t* h,
                const int32_t* params,
                size_t params_len,
                const uint8_t* dict,
                size_t dict_len) {
  if (!h) return BR_ERROR;

  if (apply_params(h, params, params_len) != 0) {
    h->err = BR_ERROR;
    return BR_ERROR;
  }

  if (dict && dict_len) {
    // Brotli requires the dictionary memory to outlive the stream; copy in.
    h->dict = (uint8_t*)malloc(dict_len);
    if (!h->dict) {
      h->err = BR_ERROR;
      return BR_ERROR;
    }
    memcpy(h->dict, dict, dict_len);
    h->dict_len = dict_len;

    if (h->is_encoder) {
      // Encoder dictionaries require `BrotliEncoderPreparedDictionary`, which
      // the upstream zlib bindings build via the (separate) `prepareDict`
      // path. Not exposed from the streaming binding; reject here so callers
      // get a clear error rather than silent corruption.
      h->err = BR_ERROR;
      return BR_ERROR;
    }

    if (!BrotliDecoderAttachDictionary(
            h->s.dec,
            BROTLI_SHARED_DICTIONARY_RAW,
            h->dict_len,
            h->dict)) {
      h->err = BR_ERROR;
      return BR_ERROR;
    }
  }

  return BR_OK;
}

uint8_t* brotli_ensure_in_buf(brotli_handle_t* h, size_t size) {
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

uint8_t* brotli_ensure_out_buf(brotli_handle_t* h, size_t size) {
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

int brotli_write(brotli_handle_t* h, int op, size_t in_len, size_t out_len) {
  if (!h) return BR_ERROR;

  const uint8_t* next_in = h->in_buf;
  uint8_t* next_out = h->out_buf;
  size_t avail_in = in_len;
  size_t avail_out = out_len;

  if (h->is_encoder) {
    BROTLI_BOOL ok = BrotliEncoderCompressStream(
        h->s.enc,
        (BrotliEncoderOperation)op,
        &avail_in, &next_in,
        &avail_out, &next_out,
        NULL);
    h->avail_in_after = avail_in;
    h->avail_out_after = avail_out;
    if (!ok) {
      h->err = BR_ERROR;
    } else if (BrotliEncoderIsFinished(h->s.enc)) {
      h->err = BR_STREAM_END;
    } else {
      h->err = BR_OK;
    }
  } else {
    BrotliDecoderResult r = BrotliDecoderDecompressStream(
        h->s.dec,
        &avail_in, &next_in,
        &avail_out, &next_out,
        NULL);
    h->avail_in_after = avail_in;
    h->avail_out_after = avail_out;
    switch (r) {
      case BROTLI_DECODER_RESULT_ERROR:
        h->err = BR_ERROR;
        break;
      case BROTLI_DECODER_RESULT_SUCCESS:
        h->err = BR_STREAM_END;
        break;
      case BROTLI_DECODER_RESULT_NEEDS_MORE_INPUT:
      case BROTLI_DECODER_RESULT_NEEDS_MORE_OUTPUT:
      default:
        h->err = BR_OK;
        break;
    }
  }

  return h->err;
}

uint32_t brotli_avail_in(brotli_handle_t* h) {
  return h ? (uint32_t)h->avail_in_after : 0;
}

uint32_t brotli_avail_out(brotli_handle_t* h) {
  return h ? (uint32_t)h->avail_out_after : 0;
}

int brotli_get_err(brotli_handle_t* h) {
  return h ? h->err : BR_ERROR;
}

const char* brotli_get_msg(brotli_handle_t* h) {
  if (!h) return NULL;
  if (h->is_encoder) return NULL;
  BrotliDecoderErrorCode code = BrotliDecoderGetErrorCode(h->s.dec);
  return BrotliDecoderErrorString(code);
}

void brotli_end(brotli_handle_t* h) {
  if (!h) return;
  if (h->is_encoder) {
    if (h->s.enc) BrotliEncoderDestroyInstance(h->s.enc);
  } else {
    if (h->s.dec) BrotliDecoderDestroyInstance(h->s.dec);
  }
  if (h->in_buf) free(h->in_buf);
  if (h->out_buf) free(h->out_buf);
  if (h->dict) free(h->dict);
  free(h);
}
