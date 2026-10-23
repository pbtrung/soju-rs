// Risk 5: libxcrypt verifies the same $2a$/$2b$/$2y$ bcrypt records as
// Go's x/crypto/bcrypt and Rust's bcrypt crate. This published test vector
// uses "U*U", cost 5 (Openwall crypt_blowfish test suite).
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
	char salt[CRYPT_GENSALT_OUTPUT_SIZE];
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
