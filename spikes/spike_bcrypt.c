// Risk 5: libxcrypt verifies the same $2a$/$2b$/$2y$ bcrypt records as
// Go's x/crypto/bcrypt and Rust's bcrypt crate. This published test vector
// uses "U*U", cost 5 (Openwall crypt_blowfish test suite).
//
// The other direction was checked when this spike was written: the $2a$
// and $2b$ hashes libxcrypt generates verify with Go's
// golang.org/x/crypto/bcrypt CompareHashAndPassword and with Rust's
// bcrypt 0.19.3 verify (soju-rs writes $2a$, like Go soju).
#include <assert.h>
#include <crypt.h>
#include <string.h>

#include "base/alloc.h"

int main(void) {
	struct crypt_data *data = xcalloc(1, sizeof(*data));
	const char *vector = "$2b$05$CCCCCCCCCCCCCCCCCCCCC.E5YPO9kmyuRGyh0XouQYb4YMJKvyOeW";
	char *hash = crypt_rn("U*U", vector, data, (int)sizeof(*data));
	assert(hash != nullptr && strcmp(hash, vector) == 0);
	for (const char *variant = "aby"; *variant != '\0'; variant++) {
		AUTOFREE char *setting = xstrdup(vector);
		setting[2] = *variant;
		hash = crypt_rn("U*U", setting, data, (int)sizeof(*data));
		assert(hash != nullptr && strcmp(hash, setting) == 0);
	}
	// Made by Go soju's library (x/crypto/bcrypt, GenerateFromPassword) and
	// by soju-rs's (bcrypt 0.19.3, in its $2a$ and default $2b$ formats)
	static const struct {
		const char *password, *hash;
	} foreign[] = {
		{"go soju password",
		 "$2a$04$kMEivx7c.9F.ygyR5WLzA.nyQCjbWFW.CKSIMEbvrxIfnqW4UIOQ."},
		{"soju-rs password",
		 "$2a$04$7GvBAaMu7MJ5iRfYvz3JNO14USBmpWuq5b0EDNh4dTf5R5onNDkOe"},
		{"soju-rs password",
		 "$2b$04$oalZTJHTaMgGTSm4Xke.refkinBz1oniTUGsViEobn7qu882ToSfO"},
	};
	for (size_t i = 0; i < sizeof(foreign) / sizeof(foreign[0]); i++) {
		hash = crypt_rn(foreign[i].password, foreign[i].hash, data, (int)sizeof(*data));
		assert(hash != nullptr && strcmp(hash, foreign[i].hash) == 0);
		hash = crypt_rn("wrong", foreign[i].hash, data, (int)sizeof(*data));
		assert(hash != nullptr && strcmp(hash, foreign[i].hash) != 0);
	}
	char salt[CRYPT_GENSALT_OUTPUT_SIZE];
	// soju-rs's format
	assert(crypt_gensalt_rn("$2a$", 10, nullptr, 0, salt, sizeof(salt)) != nullptr);
	assert(strncmp(salt, "$2a$10$", 7) == 0);
	assert(crypt_gensalt_rn("$2b$", 5, nullptr, 0, salt, sizeof(salt)) != nullptr);
	hash = crypt_rn("new password", salt, data, (int)sizeof(*data));
	assert(hash != nullptr && hash[0] != '*');
	AUTOFREE char *saved = xstrdup(hash);
	hash = crypt_rn("new password", saved, data, (int)sizeof(*data));
	assert(hash != nullptr && strcmp(saved, hash) == 0);
	explicit_bzero(data, sizeof(*data));
	xfree(data);
	return 0;
}
