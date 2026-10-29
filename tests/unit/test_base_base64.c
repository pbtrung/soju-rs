// Tests of src/base/base64.c against the base64 crate (no Rust
// counterpart)
#include "testsupport.h"

#include "base/alloc.h"
#include "base/base64.h"

static void enc_eq(enum base64_variant v, const char *in, const char *want) {
	char *s = base64_encode_cstr(v, in, strlen(in));
	assert_string_equal(s, want);
	xfree(s);
}

static void dec_eq(enum base64_variant v, const char *in, const char *want) {
	struct strbuf sb = STRBUF_INIT;
	struct err *e = nullptr;
	assert_true(base64_decode(&sb, v, str_from(in), &e));
	assert_sb_eq(&sb, want);
	strbuf_free(&sb);
}

static void dec_err(enum base64_variant v, const char *in, const char *want) {
	struct strbuf sb = STRBUF_INIT;
	struct err *e = nullptr;
	assert_false(base64_decode(&sb, v, str_from(in), &e));
	assert_string_equal(err_msg(e), want);
	assert_int_equal(sb.len, 0);
	err_free(e);
	strbuf_free(&sb);
}

static void test_encode(void **state) {
	(void)state;
	enc_eq(BASE64_STANDARD, "", "");
	enc_eq(BASE64_STANDARD, "f", "Zg==");
	enc_eq(BASE64_STANDARD, "fo", "Zm8=");
	enc_eq(BASE64_STANDARD, "foo", "Zm9v");
	enc_eq(BASE64_URL_SAFE_NO_PAD, "f", "Zg");
	enc_eq(BASE64_URL_SAFE_NO_PAD, "\xFB\xFF", "-_8");
	enc_eq(BASE64_STANDARD, "\xFB\xFF", "+/8=");
}

static void test_decode(void **state) {
	(void)state;
	dec_eq(BASE64_STANDARD, "Zm9v", "foo");
	dec_eq(BASE64_STANDARD, "Zg==", "f");
	dec_eq(BASE64_URL_SAFE_NO_PAD, "Zm8", "fo");
	dec_eq(BASE64_URL_SAFE_NO_PAD, "", "");
	dec_err(BASE64_URL_SAFE_NO_PAD, "!!!", "Invalid symbol 33, offset 0.");
	dec_err(BASE64_URL_SAFE_NO_PAD, "Zg==", "Invalid padding");
	dec_err(BASE64_STANDARD, "Zg", "Invalid padding");
	dec_err(BASE64_STANDARD, "Zg=", "Invalid padding");
	dec_err(BASE64_URL_SAFE_NO_PAD, "Z", "Invalid input length: 1");
	dec_err(BASE64_URL_SAFE_NO_PAD, "Zh",
		"Invalid last symbol 0x68 ('h') at offset 1, decoded as 0b00100001.");
	dec_err(BASE64_STANDARD, "Z=g=", "Invalid symbol 61, offset 1.");
	dec_err(BASE64_STANDARD, "Zm9vZ===", "Invalid symbol 61, offset 5.");
	dec_err(BASE64_STANDARD, "Zm9v!", "Invalid symbol 33, offset 4.");
}

T_MAIN(cmocka_unit_test(test_encode), cmocka_unit_test(test_decode))
