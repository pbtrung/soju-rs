#include "user/delivered.h"
#include "base/alloc.h"

static void free_clients(void *data) {
	struct strmap *m = data;
	strmap_free(m);
	xfree(m);
}
void delivered_init(struct delivered_store *s, enum casemapping cm) {
	cmmap_init(&s->targets, cm, free_clients);
}
void delivered_destroy(struct delivered_store *s) {
	cmmap_free(&s->targets);
}
const char *delivered_get(const struct delivered_store *s, const char *target, const char *client) {
	struct strmap *m = cmmap_get(&s->targets, target);
	const char *id = m != nullptr ? strmap_get(m, client) : nullptr;
	return id != nullptr ? id : "";
}
void delivered_set(struct delivered_store *s, const char *target, const char *client,
		   const char *id) {
	struct strmap *m = cmmap_get(&s->targets, target);
	if (m == nullptr) {
		m = xcalloc(1, sizeof(*m));
		strmap_init(m, xfree);
		cmmap_set(&s->targets, target, m);
	}
	strmap_set(m, client, xstrdup(id));
}
