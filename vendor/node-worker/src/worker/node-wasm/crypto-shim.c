// C shim exposing OpenSSL libcrypto to the JS adapter
// (src/worker/node-core/internal-binding/crypto.ts), in the same handle +
// scratch-buffer style as zlib-shim.c. Stage 0: digest one-shot, RNG, and
// timingSafeEqual — enough to validate the OpenSSL-in-wasm link and the RNG
// wiring before the rest of the binding surface is built out.

#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include <openssl/crypto.h>
#include <openssl/evp.h>
#include <openssl/hmac.h>
#include <openssl/rand.h>
#include <openssl/err.h>
#include <openssl/ec.h>
#include <openssl/objects.h>
#include <openssl/kdf.h>
#include <openssl/core_names.h>
#include <openssl/params.h>
#include <openssl/pem.h>
#include <openssl/rsa.h>
#include <openssl/bio.h>
#include <openssl/x509.h>
#include <openssl/bn.h>
#include <openssl/core_names.h>
#include <openssl/x509v3.h>
#include <limits.h>

// Entropy source. STANDALONE_WASM with FILESYSTEM=0 has no /dev/urandom or
// getrandom, so route OpenSSL's RNG to a JS env import backed by
// globalThis.crypto.getRandomValues (provided in node-wasm/loader.ts). The
// import attributes make this a wasm import from `env` rather than an
// unresolved symbol (which -sERROR_ON_UNDEFINED_SYMBOLS=1 would reject).
__attribute__((import_module("env"), import_name("js_crypto_random")))
extern int js_crypto_random(unsigned char* buf, int len);

static int wasm_rand_bytes(unsigned char* buf, int num) {
  return js_crypto_random(buf, num) == 0 ? 1 : 0;
}
static int wasm_rand_status(void) { return 1; }
static int wasm_rand_seed(const void* buf, int num) { (void)buf; (void)num; return 1; }
static int wasm_rand_add(const void* buf, int num, double r) {
  (void)buf; (void)num; (void)r; return 1;
}

static RAND_METHOD wasm_rand_method = {
  wasm_rand_seed,   // seed
  wasm_rand_bytes,  // bytes
  NULL,             // cleanup
  wasm_rand_add,    // add
  wasm_rand_bytes,  // pseudorand
  wasm_rand_status, // status
};

// OpenSSL 3.x's provider DRBG (used by RSA keygen / RAND_priv_bytes, which
// bypass the legacy RAND_METHOD above) seeds from its OS entropy source, which
// resolves to getentropy(). Provide a strong implementation backed by our JS
// entropy so the DRBG instantiates instead of failing "entropy source strength
// too weak". getentropy is limited to 256 bytes per call by contract.
int getentropy(void* buf, size_t len) {
  // POSIX caps getentropy at 256 bytes, but we control every caller and
  // OpenSSL's syscall_random may request more, so fill any length.
  return js_crypto_random((unsigned char*)buf, (int)len) == 0 ? 0 : -1;
}

void crypto_init(void) {
  RAND_set_rand_method(&wasm_rand_method);
}

// Drains the head of OpenSSL's error queue into `out`. Returns string length.
size_t crypto_last_error(char* out, size_t cap) {
  unsigned long e = ERR_get_error();
  if (e == 0) {
    if (cap) out[0] = 0;
    return 0;
  }
  ERR_error_string_n(e, out, cap);
  return strlen(out);
}

// EVP one-shot digest. Returns bytes written, or negative on error.
int crypto_md_oneshot(const char* name,
                      const uint8_t* in, size_t n,
                      uint32_t xof_len,
                      uint8_t* out, size_t cap) {
  (void)xof_len; // XOF (shake) handled later
  const EVP_MD* md = EVP_get_digestbyname(name);
  if (!md) return -1;
  if ((size_t)EVP_MD_get_size(md) > cap) return -2;
  unsigned int outlen = 0;
  if (!EVP_Digest(in, n, out, &outlen, md, NULL)) return -3;
  return (int)outlen;
}

int crypto_rand_bytes(uint8_t* out, size_t n) {
  return RAND_bytes(out, (int)n) == 1 ? 0 : -1;
}

int crypto_timing_safe_equal(const uint8_t* a, const uint8_t* b, size_t n) {
  return CRYPTO_memcmp(a, b, n) == 0 ? 1 : 0;
}

// --- streaming digest (EVP_MD_CTX) ---------------------------------------

typedef struct {
  EVP_MD_CTX* ctx;
  const EVP_MD* md;
  uint32_t xof; // requested output length for XOF (shake) digests, else 0
} md_handle_t;

md_handle_t* crypto_md_new(const char* name, uint32_t xof_len) {
  const EVP_MD* md = EVP_get_digestbyname(name);
  if (!md) return NULL;
  md_handle_t* h = (md_handle_t*)calloc(1, sizeof(md_handle_t));
  if (!h) return NULL;
  h->ctx = EVP_MD_CTX_new();
  if (!h->ctx) { free(h); return NULL; }
  if (EVP_DigestInit_ex(h->ctx, md, NULL) != 1) {
    EVP_MD_CTX_free(h->ctx);
    free(h);
    return NULL;
  }
  h->md = md;
  h->xof = xof_len;
  return h;
}

md_handle_t* crypto_md_copy(md_handle_t* src) {
  if (!src) return NULL;
  md_handle_t* h = (md_handle_t*)calloc(1, sizeof(md_handle_t));
  if (!h) return NULL;
  h->ctx = EVP_MD_CTX_new();
  if (!h->ctx) { free(h); return NULL; }
  if (EVP_MD_CTX_copy_ex(h->ctx, src->ctx) != 1) {
    EVP_MD_CTX_free(h->ctx);
    free(h);
    return NULL;
  }
  h->md = src->md;
  h->xof = src->xof;
  return h;
}

int crypto_md_update(md_handle_t* h, const uint8_t* p, size_t n) {
  return EVP_DigestUpdate(h->ctx, p, n) == 1 ? 0 : -1;
}

int crypto_md_final(md_handle_t* h, uint8_t* out, size_t cap) {
  if (EVP_MD_get_flags(h->md) & EVP_MD_FLAG_XOF) {
    size_t len = h->xof;
    if (len > cap) return -2;
    if (EVP_DigestFinalXOF(h->ctx, out, len) != 1) return -1;
    return (int)len;
  }
  if ((size_t)EVP_MD_get_size(h->md) > cap) return -2;
  unsigned int outlen = 0;
  if (EVP_DigestFinal_ex(h->ctx, out, &outlen) != 1) return -1;
  return (int)outlen;
}

void crypto_md_free(md_handle_t* h) {
  if (!h) return;
  if (h->ctx) EVP_MD_CTX_free(h->ctx);
  free(h);
}

// --- HMAC (HMAC_CTX) ------------------------------------------------------

HMAC_CTX* crypto_hmac_new(const char* md_name, const uint8_t* key, size_t keylen) {
  const EVP_MD* md = EVP_get_digestbyname(md_name);
  if (!md) return NULL;
  HMAC_CTX* ctx = HMAC_CTX_new();
  if (!ctx) return NULL;
  if (HMAC_Init_ex(ctx, key, (int)keylen, md, NULL) != 1) {
    HMAC_CTX_free(ctx);
    return NULL;
  }
  return ctx;
}

int crypto_hmac_update(HMAC_CTX* ctx, const uint8_t* p, size_t n) {
  return HMAC_Update(ctx, p, n) == 1 ? 0 : -1;
}

int crypto_hmac_final(HMAC_CTX* ctx, uint8_t* out, size_t cap) {
  if (cap < EVP_MAX_MD_SIZE) return -2;
  unsigned int outlen = 0;
  if (HMAC_Final(ctx, out, &outlen) != 1) return -1;
  return (int)outlen;
}

void crypto_hmac_free(HMAC_CTX* ctx) {
  if (ctx) HMAC_CTX_free(ctx);
}

// --- capability enumeration ----------------------------------------------
// Names are written newline-separated into `out`; the return value is the
// total byte length (the JS side slices and splits on '\n'). Truncates
// silently if the (generous) cap is exceeded.

typedef struct { char* buf; size_t len; size_t cap; } strbuf_t;

static void sb_add(strbuf_t* sb, const char* s) {
  if (!s) return;
  size_t sl = strlen(s);
  if (sb->len + sl + 1 > sb->cap) return;
  memcpy(sb->buf + sb->len, s, sl);
  sb->len += sl;
  sb->buf[sb->len++] = '\n';
}

// Collect every registered alias (SHA2-256, SHA-256, SHA256, ...) so the
// lowercased JS list includes node's legacy names like "sha256".
static void name_cb(const char* name, void* arg) {
  sb_add((strbuf_t*)arg, name);
}
static void collect_md(EVP_MD* md, void* arg) {
  EVP_MD_names_do_all(md, name_cb, arg);
}

int crypto_get_hashes(char* out, size_t cap) {
  strbuf_t sb = { out, 0, cap };
  EVP_MD_do_all_provided(NULL, collect_md, &sb);
  return (int)sb.len;
}

static void collect_cipher(EVP_CIPHER* c, void* arg) {
  EVP_CIPHER_names_do_all(c, name_cb, arg);
}

int crypto_get_ciphers(char* out, size_t cap) {
  strbuf_t sb = { out, 0, cap };
  EVP_CIPHER_do_all_provided(NULL, collect_cipher, &sb);
  return (int)sb.len;
}

int crypto_get_curves(char* out, size_t cap) {
  size_t n = EC_get_builtin_curves(NULL, 0);
  if (n == 0) return 0;
  EC_builtin_curve* curves = (EC_builtin_curve*)malloc(n * sizeof(EC_builtin_curve));
  if (!curves) return -1;
  EC_get_builtin_curves(curves, n);
  strbuf_t sb = { out, 0, cap };
  for (size_t i = 0; i < n; i++) {
    sb_add(&sb, OBJ_nid2sn(curves[i].nid));
  }
  free(curves);
  return (int)sb.len;
}

// --- key derivation -------------------------------------------------------

int crypto_pbkdf2(const uint8_t* pass, size_t plen,
                  const uint8_t* salt, size_t slen,
                  int iterations, const char* md_name,
                  uint8_t* out, size_t keylen) {
  const EVP_MD* md = EVP_get_digestbyname(md_name);
  if (!md) return -1;
  if (keylen == 0) return 0;
  return PKCS5_PBKDF2_HMAC((const char*)pass, (int)plen, salt, (int)slen,
                           iterations, md, (int)keylen, out) == 1 ? 0 : -2;
}

// N/r/p/maxmem are uint64_t in OpenSSL; accept doubles to avoid i64 wasm
// boundary marshalling (the ranges fit f64 exactly).
int crypto_scrypt(const uint8_t* pass, size_t plen,
                  const uint8_t* salt, size_t slen,
                  double N, double r, double p, double maxmem,
                  uint8_t* out, size_t keylen) {
  if (keylen == 0) return 0;
  return EVP_PBE_scrypt((const char*)pass, plen, salt, slen,
                        (uint64_t)N, (uint64_t)r, (uint64_t)p, (uint64_t)maxmem,
                        out, keylen) == 1 ? 0 : -1;
}

int crypto_hkdf(const char* md_name,
                const uint8_t* ikm, size_t ikmlen,
                const uint8_t* salt, size_t saltlen,
                const uint8_t* info, size_t infolen,
                uint8_t* out, size_t outlen) {
  EVP_KDF* kdf = EVP_KDF_fetch(NULL, "HKDF", NULL);
  if (!kdf) return -1;
  EVP_KDF_CTX* kctx = EVP_KDF_CTX_new(kdf);
  EVP_KDF_free(kdf);
  if (!kctx) return -2;

  OSSL_PARAM params[5];
  int i = 0;
  params[i++] = OSSL_PARAM_construct_utf8_string(OSSL_KDF_PARAM_DIGEST,
                                                 (char*)md_name, 0);
  params[i++] = OSSL_PARAM_construct_octet_string(OSSL_KDF_PARAM_KEY,
                                                  (void*)ikm, ikmlen);
  params[i++] = OSSL_PARAM_construct_octet_string(OSSL_KDF_PARAM_SALT,
                                                  (void*)salt, saltlen);
  params[i++] = OSSL_PARAM_construct_octet_string(OSSL_KDF_PARAM_INFO,
                                                  (void*)info, infolen);
  params[i] = OSSL_PARAM_construct_end();

  int rc = EVP_KDF_derive(kctx, out, outlen, params);
  EVP_KDF_CTX_free(kctx);
  return rc == 1 ? 0 : -3;
}

// --- symmetric ciphers (EVP_CIPHER_CTX, incl. AEAD) -----------------------

typedef struct {
  EVP_CIPHER_CTX* ctx;
  int encrypt;
  int aead;
  int ccm;
  int auth_tag_len; // requested tag length, -1 if unset
} cipher_handle_t;

void* crypto_cipher_new(int encrypt, const char* name,
                        const uint8_t* key, size_t keylen,
                        const uint8_t* iv, size_t ivlen,
                        int auth_tag_len) {
  const EVP_CIPHER* c = EVP_get_cipherbyname(name);
  if (!c) return NULL;
  cipher_handle_t* h = (cipher_handle_t*)calloc(1, sizeof(cipher_handle_t));
  if (!h) return NULL;
  h->ctx = EVP_CIPHER_CTX_new();
  if (!h->ctx) { free(h); return NULL; }
  h->encrypt = encrypt;
  h->auth_tag_len = auth_tag_len;
  h->aead = (EVP_CIPHER_get_flags(c) & EVP_CIPH_FLAG_AEAD_CIPHER) != 0;
  h->ccm = (EVP_CIPHER_get_mode(c) == EVP_CIPH_CCM_MODE);

  // Set cipher first so AEAD IV length can be adjusted before key/iv.
  if (EVP_CipherInit_ex(h->ctx, c, NULL, NULL, NULL, encrypt) != 1) goto fail;
  if (h->aead && iv && ivlen > 0) {
    if (EVP_CIPHER_CTX_ctrl(h->ctx, EVP_CTRL_AEAD_SET_IVLEN, (int)ivlen, NULL) != 1)
      goto fail;
  }
  if (h->ccm && auth_tag_len > 0) {
    if (EVP_CIPHER_CTX_ctrl(h->ctx, EVP_CTRL_AEAD_SET_TAG, auth_tag_len, NULL) != 1)
      goto fail;
  }
  if (EVP_CipherInit_ex(h->ctx, NULL, NULL, key, iv, encrypt) != 1) goto fail;
  return h;
fail:
  EVP_CIPHER_CTX_free(h->ctx);
  free(h);
  return NULL;
}

int crypto_cipher_update(void* handle, const uint8_t* in, size_t inlen,
                         uint8_t* out, size_t cap) {
  cipher_handle_t* h = (cipher_handle_t*)handle;
  (void)cap;
  int outl = 0;
  if (EVP_CipherUpdate(h->ctx, out, &outl, in, (int)inlen) != 1) return -1;
  return outl;
}

int crypto_cipher_final(void* handle, uint8_t* out, size_t cap) {
  cipher_handle_t* h = (cipher_handle_t*)handle;
  (void)cap;
  int outl = 0;
  // Returns 0 on AEAD tag verification failure (decrypt).
  if (EVP_CipherFinal_ex(h->ctx, out, &outl) != 1) return -1;
  return outl;
}

int crypto_cipher_set_aad(void* handle, const uint8_t* aad, size_t aadlen,
                          int plaintext_len) {
  cipher_handle_t* h = (cipher_handle_t*)handle;
  int outl = 0;
  if (h->ccm && plaintext_len >= 0) {
    if (EVP_CipherUpdate(h->ctx, NULL, &outl, NULL, plaintext_len) != 1) return -1;
  }
  if (EVP_CipherUpdate(h->ctx, NULL, &outl, aad, (int)aadlen) != 1) return -1;
  return 0;
}

int crypto_cipher_get_auth_tag(void* handle, uint8_t* out, size_t cap) {
  cipher_handle_t* h = (cipher_handle_t*)handle;
  int taglen = h->auth_tag_len > 0 ? h->auth_tag_len : 16;
  if ((size_t)taglen > cap) return -1;
  if (EVP_CIPHER_CTX_ctrl(h->ctx, EVP_CTRL_AEAD_GET_TAG, taglen, out) != 1) return -1;
  return taglen;
}

int crypto_cipher_set_auth_tag(void* handle, const uint8_t* tag, size_t taglen) {
  cipher_handle_t* h = (cipher_handle_t*)handle;
  if (EVP_CIPHER_CTX_ctrl(h->ctx, EVP_CTRL_AEAD_SET_TAG, (int)taglen, (void*)tag) != 1)
    return -1;
  return 0;
}

int crypto_cipher_set_auto_padding(void* handle, int pad) {
  cipher_handle_t* h = (cipher_handle_t*)handle;
  return EVP_CIPHER_CTX_set_padding(h->ctx, pad) == 1 ? 0 : -1;
}

void crypto_cipher_free(void* handle) {
  cipher_handle_t* h = (cipher_handle_t*)handle;
  if (!h) return;
  if (h->ctx) EVP_CIPHER_CTX_free(h->ctx);
  free(h);
}

// Fills out[0..4] = {nid, blockSize, ivLength, keyLength, mode} and the
// canonical name. Returns 0 on success, -1 if the cipher is unknown.
int crypto_cipher_info(const char* name, int32_t* out, char* nameout, size_t namecap) {
  const EVP_CIPHER* c = EVP_get_cipherbyname(name);
  if (!c) return -1;
  out[0] = EVP_CIPHER_get_nid(c);
  out[1] = EVP_CIPHER_get_block_size(c);
  out[2] = EVP_CIPHER_get_iv_length(c);
  out[3] = EVP_CIPHER_get_key_length(c);
  out[4] = EVP_CIPHER_get_mode(c);
  const char* nm = EVP_CIPHER_get0_name(c);
  if (nm && nameout && namecap) {
    size_t n = strlen(nm);
    if (n >= namecap) n = namecap - 1;
    memcpy(nameout, nm, n);
    nameout[n] = 0;
  }
  return 0;
}

// --- asymmetric keys: parse / export / generate / sign / verify ----------
// Keys are held as EVP_PKEY* (returned as opaque wasm pointers). Sentinel for
// "option not provided" on the PSS salt length (real values include -1/-2/-3).
#define SALTLEN_UNSET 0x7fffffff

static char* dupz(const uint8_t* p, size_t n) {
  if (!p || n == 0) return NULL;
  char* z = (char*)malloc(n + 1);
  if (!z) return NULL;
  memcpy(z, p, n);
  z[n] = 0;
  return z;
}

static int bio_to_out(BIO* bio, uint8_t* out, size_t cap) {
  BUF_MEM* bptr = NULL;
  BIO_get_mem_ptr(bio, &bptr);
  if (!bptr) return -1;
  if (bptr->length > cap) return -((int)bptr->length); // signal needed size
  memcpy(out, bptr->data, bptr->length);
  return (int)bptr->length;
}

// key_type: 1=public, 2=private. format: 0=DER, 1=PEM.
// enc_type: 0=PKCS1, 1=PKCS8, 2=SPKI, 3=SEC1.
void* crypto_pkey_parse(int key_type, const uint8_t* data, size_t len,
                        int format, int enc_type,
                        const uint8_t* pass, size_t passlen) {
  char* passz = dupz(pass, passlen);
  EVP_PKEY* pkey = NULL;
  if (format == 1) { // PEM (header self-describes the encoding)
    BIO* bio = BIO_new_mem_buf(data, (int)len);
    if (bio) {
      if (key_type == 2) {
        pkey = PEM_read_bio_PrivateKey(bio, NULL, NULL, passz);
      } else {
        pkey = PEM_read_bio_PUBKEY(bio, NULL, NULL, NULL);
        if (!pkey) {
          BIO_free(bio);
          bio = BIO_new_mem_buf(data, (int)len);
          RSA* r = PEM_read_bio_RSAPublicKey(bio, NULL, NULL, NULL);
          if (r) { pkey = EVP_PKEY_new(); EVP_PKEY_assign_RSA(pkey, r); }
        }
      }
      BIO_free(bio);
    }
  } else { // DER
    const unsigned char* p = data;
    if (key_type == 2) {
      pkey = d2i_AutoPrivateKey(NULL, &p, (long)len);
    } else if (enc_type == 0) { // PKCS1 RSA public
      RSA* r = d2i_RSAPublicKey(NULL, &p, (long)len);
      if (r) { pkey = EVP_PKEY_new(); EVP_PKEY_assign_RSA(pkey, r); }
    } else { // SPKI
      pkey = d2i_PUBKEY(NULL, &p, (long)len);
    }
  }
  free(passz);
  return pkey;
}

void crypto_pkey_free(void* pkey) {
  if (pkey) EVP_PKEY_free((EVP_PKEY*)pkey);
}

// Bump the refcount so a generated keypair can back two KeyObjectHandles
// (public + private) that each free independently.
void* crypto_pkey_up_ref(void* pkey) {
  if (pkey) EVP_PKEY_up_ref((EVP_PKEY*)pkey);
  return pkey;
}

int crypto_pkey_type(void* pkey_v, char* out, size_t cap) {
  EVP_PKEY* pkey = (EVP_PKEY*)pkey_v;
  const char* s;
  switch (EVP_PKEY_get_base_id(pkey)) {
    case EVP_PKEY_RSA: s = "rsa"; break;
    case EVP_PKEY_RSA_PSS: s = "rsa-pss"; break;
    case EVP_PKEY_EC: s = "ec"; break;
    case EVP_PKEY_ED25519: s = "ed25519"; break;
    case EVP_PKEY_ED448: s = "ed448"; break;
    case EVP_PKEY_X25519: s = "x25519"; break;
    case EVP_PKEY_X448: s = "x448"; break;
    case EVP_PKEY_DSA: s = "dsa"; break;
    case EVP_PKEY_DH: s = "dh"; break;
    default: s = "unknown";
  }
  size_t n = strlen(s);
  if (n >= cap) n = cap - 1;
  memcpy(out, s, n);
  out[n] = 0;
  return (int)n;
}

int crypto_pkey_export(void* pkey_v, int key_type, int format, int enc_type,
                       const char* cipher_name, const uint8_t* pass, size_t passlen,
                       uint8_t* out, size_t cap) {
  EVP_PKEY* pkey = (EVP_PKEY*)pkey_v;
  BIO* bio = BIO_new(BIO_s_mem());
  if (!bio) return -1;
  char* passz = dupz(pass, passlen);
  const EVP_CIPHER* enc = (cipher_name && cipher_name[0]) ? EVP_get_cipherbyname(cipher_name) : NULL;
  int ok = 0;
  if (key_type == 1) { // public
    if (enc_type == 0) { // PKCS1 (RSA)
      RSA* r = EVP_PKEY_get1_RSA(pkey);
      if (r) { ok = format == 1 ? PEM_write_bio_RSAPublicKey(bio, r) : i2d_RSAPublicKey_bio(bio, r); RSA_free(r); }
    } else { // SPKI
      ok = format == 1 ? PEM_write_bio_PUBKEY(bio, pkey) : i2d_PUBKEY_bio(bio, pkey);
    }
  } else { // private
    if (enc_type == 0) { // PKCS1 (RSA)
      RSA* r = EVP_PKEY_get1_RSA(pkey);
      if (r) {
        ok = format == 1
          ? PEM_write_bio_RSAPrivateKey(bio, r, enc, NULL, 0, NULL, passz)
          : i2d_RSAPrivateKey_bio(bio, r);
        RSA_free(r);
      }
    } else if (enc_type == 3) { // SEC1 (EC)
      EC_KEY* ec = EVP_PKEY_get1_EC_KEY(pkey);
      if (ec) {
        ok = format == 1
          ? PEM_write_bio_ECPrivateKey(bio, ec, enc, NULL, 0, NULL, passz)
          : i2d_ECPrivateKey_bio(bio, ec);
        EC_KEY_free(ec);
      }
    } else { // PKCS8
      int plen = passz ? (int)passlen : 0;
      ok = format == 1
        ? PEM_write_bio_PKCS8PrivateKey(bio, pkey, enc, passz, plen, NULL, NULL)
        : i2d_PKCS8PrivateKey_bio(bio, pkey, enc, passz, plen, NULL, NULL);
    }
  }
  int ret = ok ? bio_to_out(bio, out, cap) : -2;
  BIO_free(bio);
  free(passz);
  return ret;
}

// Writes a small JSON blob of key details. publicExponent is emitted as a
// decimal string (JS converts to BigInt).
int crypto_pkey_detail(void* pkey_v, char* out, size_t cap) {
  EVP_PKEY* pkey = (EVP_PKEY*)pkey_v;
  int id = EVP_PKEY_get_base_id(pkey);
  int n = 0;
  if (id == EVP_PKEY_RSA || id == EVP_PKEY_RSA_PSS) {
    BIGNUM* e = NULL;
    EVP_PKEY_get_bn_param(pkey, OSSL_PKEY_PARAM_RSA_E, &e);
    // publicExponent is emitted as big-endian hex; the JS layer turns it into
    // an ArrayBuffer (node's normalizeKeyDetails does new Uint8Array(...)).
    char* ehex = e ? BN_bn2hex(e) : NULL;
    n = snprintf(out, cap, "{\"modulusLength\":%d,\"publicExponentHex\":\"%s\"}",
                 EVP_PKEY_get_bits(pkey), ehex ? ehex : "010001");
    if (ehex) OPENSSL_free(ehex);
    if (e) BN_free(e);
  } else if (id == EVP_PKEY_EC) {
    char gname[80] = {0};
    size_t glen = 0;
    EVP_PKEY_get_utf8_string_param(pkey, OSSL_PKEY_PARAM_GROUP_NAME, gname, sizeof(gname), &glen);
    n = snprintf(out, cap, "{\"namedCurve\":\"%s\"}", gname);
  } else {
    n = snprintf(out, cap, "{}");
  }
  return n < 0 ? -1 : n;
}

void* crypto_generate_rsa(int bits, unsigned int e) {
  EVP_PKEY_CTX* ctx = EVP_PKEY_CTX_new_id(EVP_PKEY_RSA, NULL);
  if (!ctx) return NULL;
  EVP_PKEY* pkey = NULL;
  if (EVP_PKEY_keygen_init(ctx) == 1 &&
      EVP_PKEY_CTX_set_rsa_keygen_bits(ctx, bits) == 1) {
    BIGNUM* be = BN_new();
    BN_set_word(be, e ? e : 65537);
    EVP_PKEY_CTX_set1_rsa_keygen_pubexp(ctx, be);
    BN_free(be);
    EVP_PKEY_keygen(ctx, &pkey);
  }
  EVP_PKEY_CTX_free(ctx);
  return pkey;
}

void* crypto_generate_ec(const char* curve) {
  int nid = OBJ_sn2nid(curve);
  if (nid == NID_undef) nid = EC_curve_nist2nid(curve);
  if (nid == NID_undef) return NULL;
  EVP_PKEY_CTX* ctx = EVP_PKEY_CTX_new_id(EVP_PKEY_EC, NULL);
  EVP_PKEY* pkey = NULL;
  if (ctx && EVP_PKEY_keygen_init(ctx) == 1 &&
      EVP_PKEY_CTX_set_ec_paramgen_curve_nid(ctx, nid) == 1) {
    EVP_PKEY_keygen(ctx, &pkey);
  }
  if (ctx) EVP_PKEY_CTX_free(ctx);
  return pkey;
}

void* crypto_generate_ed(int evp_id) {
  EVP_PKEY_CTX* ctx = EVP_PKEY_CTX_new_id(evp_id, NULL);
  EVP_PKEY* pkey = NULL;
  if (ctx && EVP_PKEY_keygen_init(ctx) == 1) {
    EVP_PKEY_keygen(ctx, &pkey);
  }
  if (ctx) EVP_PKEY_CTX_free(ctx);
  return pkey;
}

static void apply_rsa_opts(EVP_PKEY_CTX* pctx, int rsa_padding, int pss_saltlen) {
  if (rsa_padding > 0) {
    EVP_PKEY_CTX_set_rsa_padding(pctx, rsa_padding);
    if (rsa_padding == RSA_PKCS1_PSS_PADDING && pss_saltlen != SALTLEN_UNSET) {
      EVP_PKEY_CTX_set_rsa_pss_saltlen(pctx, pss_saltlen);
    }
  }
}

// md_name NULL/empty => no prehash (Ed25519/Ed448). dsa_sig_enc: 0=DER, 1=P1363.
int crypto_pkey_sign(void* pkey_v, const char* md_name,
                     const uint8_t* data, size_t datalen,
                     int rsa_padding, int pss_saltlen, int dsa_sig_enc,
                     uint8_t* out, size_t cap) {
  EVP_PKEY* pkey = (EVP_PKEY*)pkey_v;
  const EVP_MD* md = (md_name && md_name[0]) ? EVP_get_digestbyname(md_name) : NULL;
  if (md_name && md_name[0] && !md) return -1;
  EVP_MD_CTX* mdctx = EVP_MD_CTX_new();
  if (!mdctx) return -2;
  EVP_PKEY_CTX* pctx = NULL;
  unsigned char* sig = NULL;
  int ret = -3;
  size_t siglen = 0;
  if (EVP_DigestSignInit(mdctx, &pctx, md, NULL, pkey) != 1) goto done;
  apply_rsa_opts(pctx, rsa_padding, pss_saltlen);
  if (EVP_DigestSign(mdctx, NULL, &siglen, data, datalen) != 1) goto done;
  sig = (unsigned char*)malloc(siglen);
  if (!sig) { ret = -4; goto done; }
  if (EVP_DigestSign(mdctx, sig, &siglen, data, datalen) != 1) goto done;
  if (dsa_sig_enc == 1 && EVP_PKEY_get_base_id(pkey) == EVP_PKEY_EC) {
    // Convert DER ECDSA signature to fixed-size r||s (IEEE P1363 / JOSE).
    const unsigned char* p = sig;
    ECDSA_SIG* s = d2i_ECDSA_SIG(NULL, &p, (long)siglen);
    if (!s) goto done;
    int fieldlen = (EVP_PKEY_get_bits(pkey) + 7) / 8;
    int rawlen = fieldlen * 2;
    if ((size_t)rawlen > cap) { ECDSA_SIG_free(s); ret = -rawlen; goto done; }
    const BIGNUM *r, *ss;
    ECDSA_SIG_get0(s, &r, &ss);
    memset(out, 0, rawlen);
    BN_bn2binpad(r, out, fieldlen);
    BN_bn2binpad(ss, out + fieldlen, fieldlen);
    ECDSA_SIG_free(s);
    ret = rawlen;
    goto done;
  }
  if (siglen > cap) { ret = -((int)siglen); goto done; }
  memcpy(out, sig, siglen);
  ret = (int)siglen;
done:
  if (sig) free(sig);
  EVP_MD_CTX_free(mdctx);
  return ret;
}

// Returns 1 valid, 0 invalid, <0 error.
int crypto_pkey_verify(void* pkey_v, const char* md_name,
                       const uint8_t* data, size_t datalen,
                       const uint8_t* sig, size_t siglen,
                       int rsa_padding, int pss_saltlen, int dsa_sig_enc) {
  EVP_PKEY* pkey = (EVP_PKEY*)pkey_v;
  const EVP_MD* md = (md_name && md_name[0]) ? EVP_get_digestbyname(md_name) : NULL;
  if (md_name && md_name[0] && !md) return -1;
  EVP_MD_CTX* mdctx = EVP_MD_CTX_new();
  if (!mdctx) return -2;
  EVP_PKEY_CTX* pctx = NULL;
  unsigned char* derbuf = NULL;
  int ret = -3;
  if (EVP_DigestVerifyInit(mdctx, &pctx, md, NULL, pkey) != 1) goto done;
  apply_rsa_opts(pctx, rsa_padding, pss_saltlen);
  if (dsa_sig_enc == 1 && EVP_PKEY_get_base_id(pkey) == EVP_PKEY_EC) {
    int fieldlen = (EVP_PKEY_get_bits(pkey) + 7) / 8;
    if (siglen != (size_t)(fieldlen * 2)) { ret = 0; goto done; }
    ECDSA_SIG* s = ECDSA_SIG_new();
    BIGNUM* r = BN_bin2bn(sig, fieldlen, NULL);
    BIGNUM* ss = BN_bin2bn(sig + fieldlen, fieldlen, NULL);
    ECDSA_SIG_set0(s, r, ss);
    int derlen = i2d_ECDSA_SIG(s, &derbuf);
    ECDSA_SIG_free(s);
    if (derlen <= 0) { ret = -4; goto done; }
    ret = EVP_DigestVerify(mdctx, derbuf, derlen, data, datalen) == 1 ? 1 : 0;
    goto done;
  }
  ret = EVP_DigestVerify(mdctx, sig, siglen, data, datalen) == 1 ? 1 : 0;
done:
  if (derbuf) OPENSSL_free(derbuf);
  EVP_MD_CTX_free(mdctx);
  return ret;
}

// secret key generation (generateKey): just fills `out` with random bytes.
int crypto_generate_secret(uint8_t* out, size_t nbytes) {
  return RAND_bytes(out, (int)nbytes) == 1 ? 0 : -1;
}

// --- X.509 certificates ---------------------------------------------------

void* crypto_x509_parse(const uint8_t* data, size_t len) {
  X509* x = NULL;
  BIO* bio = BIO_new_mem_buf(data, (int)len);
  if (bio) {
    x = PEM_read_bio_X509(bio, NULL, NULL, NULL);
    BIO_free(bio);
  }
  if (!x) {
    const unsigned char* p = data;
    x = d2i_X509(NULL, &p, (long)len);
  }
  return x;
}

void crypto_x509_free(void* x) {
  if (x) X509_free((X509*)x);
}

// which: 0=subject, 1=issuer. Node-style multiline "SN=value\n..." output.
int crypto_x509_name(void* x509, int which, char* out, size_t cap) {
  X509* x = (X509*)x509;
  X509_NAME* nm = which == 0 ? X509_get_subject_name(x) : X509_get_issuer_name(x);
  if (!nm) return -1;
  BIO* bio = BIO_new(BIO_s_mem());
  if (!bio) return -1;
  unsigned long flags = XN_FLAG_SEP_MULTILINE | XN_FLAG_FN_SN |
                        ASN1_STRFLGS_ESC_CTRL | ASN1_STRFLGS_UTF8_CONVERT;
  X509_NAME_print_ex(bio, nm, 0, flags);
  int ret = bio_to_out(bio, (uint8_t*)out, cap);
  BIO_free(bio);
  return ret;
}

int crypto_x509_fingerprint(void* x509, const char* md_name, char* out, size_t cap) {
  X509* x = (X509*)x509;
  const EVP_MD* md = EVP_get_digestbyname(md_name);
  if (!md) return -1;
  unsigned char buf[EVP_MAX_MD_SIZE];
  unsigned int n = 0;
  if (X509_digest(x, md, buf, &n) != 1) return -1;
  // format "AA:BB:CC..."
  size_t need = n * 3 - 1;
  if (need >= cap) return -((int)need + 1);
  static const char hexd[] = "0123456789ABCDEF";
  size_t o = 0;
  for (unsigned int i = 0; i < n; i++) {
    if (i) out[o++] = ':';
    out[o++] = hexd[buf[i] >> 4];
    out[o++] = hexd[buf[i] & 0xf];
  }
  out[o] = 0;
  return (int)o;
}

// which: 0=notBefore, 1=notAfter
int crypto_x509_valid(void* x509, int which, char* out, size_t cap) {
  X509* x = (X509*)x509;
  const ASN1_TIME* t = which == 0 ? X509_get0_notBefore(x) : X509_get0_notAfter(x);
  if (!t) return -1;
  BIO* bio = BIO_new(BIO_s_mem());
  if (!bio) return -1;
  ASN1_TIME_print(bio, t);
  int ret = bio_to_out(bio, (uint8_t*)out, cap);
  BIO_free(bio);
  return ret;
}

int crypto_x509_serial(void* x509, char* out, size_t cap) {
  X509* x = (X509*)x509;
  ASN1_INTEGER* s = X509_get_serialNumber(x);
  BIGNUM* bn = s ? ASN1_INTEGER_to_BN(s, NULL) : NULL;
  if (!bn) return -1;
  char* hex = BN_bn2hex(bn);
  BN_free(bn);
  if (!hex) return -1;
  size_t n = strlen(hex);
  int ret;
  if (n >= cap) ret = -((int)n + 1);
  else { memcpy(out, hex, n); out[n] = 0; ret = (int)n; }
  OPENSSL_free(hex);
  return ret;
}

// Prints an extension (by NID) via X509V3_EXT_print, e.g. subjectAltName.
static int x509_ext_print(X509* x, int nid, char* out, size_t cap) {
  int idx = X509_get_ext_by_NID(x, nid, -1);
  if (idx < 0) return 0; // not present -> empty
  X509_EXTENSION* ext = X509_get_ext(x, idx);
  if (!ext) return 0;
  BIO* bio = BIO_new(BIO_s_mem());
  if (!bio) return -1;
  if (!X509V3_EXT_print(bio, ext, 0, 0)) {
    // fall back to raw octet dump
    ASN1_STRING_print(bio, X509_EXTENSION_get_data(ext));
  }
  int ret = bio_to_out(bio, (uint8_t*)out, cap);
  BIO_free(bio);
  return ret;
}

int crypto_x509_subject_alt_name(void* x509, char* out, size_t cap) {
  return x509_ext_print((X509*)x509, NID_subject_alt_name, out, cap);
}
int crypto_x509_info_access(void* x509, char* out, size_t cap) {
  return x509_ext_print((X509*)x509, NID_info_access, out, cap);
}

int crypto_x509_sig_alg(void* x509, int oid, char* out, size_t cap) {
  X509* x = (X509*)x509;
  const X509_ALGOR* alg = NULL;
  X509_get0_signature(NULL, &alg, x);
  if (!alg) return -1;
  const ASN1_OBJECT* obj = NULL;
  X509_ALGOR_get0(&obj, NULL, NULL, alg);
  if (!obj) return -1;
  int n = OBJ_obj2txt(out, (int)cap, obj, oid ? 1 : 0);
  return n < 0 ? -1 : n;
}

int crypto_x509_raw(void* x509, uint8_t* out, size_t cap) {
  X509* x = (X509*)x509;
  int len = i2d_X509(x, NULL);
  if (len <= 0) return -1;
  if ((size_t)len > cap) return -len;
  unsigned char* p = out;
  return i2d_X509(x, &p);
}

int crypto_x509_pem(void* x509, char* out, size_t cap) {
  BIO* bio = BIO_new(BIO_s_mem());
  if (!bio) return -1;
  int ok = PEM_write_bio_X509(bio, (X509*)x509);
  int ret = ok ? bio_to_out(bio, (uint8_t*)out, cap) : -1;
  BIO_free(bio);
  return ret;
}

void* crypto_x509_public_key(void* x509) {
  return X509_get_pubkey((X509*)x509); // EVP_PKEY* with +1 ref
}

int crypto_x509_check_host(void* x509, const char* name, int flags) {
  return X509_check_host((X509*)x509, name, 0, (unsigned int)flags, NULL) == 1 ? 1 : 0;
}
int crypto_x509_check_email(void* x509, const char* email, int flags) {
  return X509_check_email((X509*)x509, email, 0, (unsigned int)flags) == 1 ? 1 : 0;
}
int crypto_x509_check_ip(void* x509, const char* ip, int flags) {
  return X509_check_ip_asc((X509*)x509, ip, (unsigned int)flags) == 1 ? 1 : 0;
}
int crypto_x509_check_ca(void* x509) {
  return X509_check_ca((X509*)x509) > 0 ? 1 : 0;
}
int crypto_x509_verify(void* x509, void* pkey) {
  return X509_verify((X509*)x509, (EVP_PKEY*)pkey) == 1 ? 1 : 0;
}
int crypto_x509_check_issued(void* issuer, void* subject) {
  return X509_check_issued((X509*)issuer, (X509*)subject) == X509_V_OK ? 1 : 0;
}

// Self-check used by Stage 0 to confirm the OpenSSL link works end-to-end.
// Returns 0 on success. sha256("abc") begins ba7816bf...
int crypto_smoke_test(void) {
  static const uint8_t abc[3] = { 'a', 'b', 'c' };
  uint8_t out[32];
  unsigned int outlen = 0;
  const EVP_MD* md = EVP_get_digestbyname("sha256");
  if (!md) return -1;
  if (!EVP_Digest(abc, 3, out, &outlen, md, NULL)) return -2;
  if (outlen != 32) return -3;
  if (out[0] != 0xba || out[1] != 0x78 || out[2] != 0x16 || out[3] != 0xbf) return -4;
  return 0;
}
