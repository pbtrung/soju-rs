#include "db/hrana.h"

#include "base/alloc.h"

// Phase 9b, step 2 implements the protocol
struct hrana {
	int unused;
};

void hrana_free(struct hrana *h) {
	xfree(h);
}

bool hrana_execute(struct hrana *h, const char *sql, const struct db_bind *binds, size_t n,
		   strvec *columns, void *rows, int64_t *last_id, int64_t *changes,
		   struct err **err) {
	(void)h, (void)sql, (void)binds, (void)n, (void)columns, (void)rows, (void)last_id;
	(void)changes;
	return err_set(err, 0, "Turso isn't supported yet");
}

bool hrana_sequence(struct hrana *h, const char *sql, struct err **err) {
	(void)h, (void)sql;
	return err_set(err, 0, "Turso isn't supported yet");
}

bool hrana_begin(struct hrana *h, struct err **err) {
	(void)h;
	return err_set(err, 0, "Turso isn't supported yet");
}

bool hrana_commit(struct hrana *h, struct err **err) {
	(void)h;
	return err_set(err, 0, "Turso isn't supported yet");
}

void hrana_rollback(struct hrana *h) {
	(void)h;
}
