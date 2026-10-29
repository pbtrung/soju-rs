// SQLite fault injection (PLAN.md 2.3 L4): a VFS shim fails the k-th write
// or sync with SQLITE_IOERR or SQLITE_FULL, for every k of an operation.
// The operation must fail cleanly (or succeed), the database stays usable,
// and nothing leaks (every statement finalized: db_close checks).
#include "testsupport.h"

#include <sqlite3.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

#include "base/alloc.h"
#include "db/db.h"

struct faulty_file {
	sqlite3_file base;
	sqlite3_file *real;
};

static sqlite3_vfs *real_vfs;
static int countdown; // 0: no faults; fails when it reaches 0 from 1
static int fault_code;
static int faults;

static bool fault(void) {
	if (countdown <= 0) {
		return false;
	}
	countdown--;
	if (countdown > 0) {
		return false;
	}
	faults++;
	return true;
}

#define REAL(f) (((struct faulty_file *)(f))->real)

static int f_close(sqlite3_file *f) {
	int rc = REAL(f)->pMethods->xClose(REAL(f));
	xfree(REAL(f));
	return rc;
}

static int f_read(sqlite3_file *f, void *p, int n, sqlite3_int64 off) {
	return REAL(f)->pMethods->xRead(REAL(f), p, n, off);
}

static int f_write(sqlite3_file *f, const void *p, int n, sqlite3_int64 off) {
	if (fault()) {
		return fault_code;
	}
	return REAL(f)->pMethods->xWrite(REAL(f), p, n, off);
}

static int f_truncate(sqlite3_file *f, sqlite3_int64 size) {
	return REAL(f)->pMethods->xTruncate(REAL(f), size);
}

static int f_sync(sqlite3_file *f, int flags) {
	if (fault()) {
		return SQLITE_IOERR_FSYNC;
	}
	return REAL(f)->pMethods->xSync(REAL(f), flags);
}

static int f_size(sqlite3_file *f, sqlite3_int64 *size) {
	return REAL(f)->pMethods->xFileSize(REAL(f), size);
}

static int f_lock(sqlite3_file *f, int l) {
	return REAL(f)->pMethods->xLock(REAL(f), l);
}

static int f_unlock(sqlite3_file *f, int l) {
	return REAL(f)->pMethods->xUnlock(REAL(f), l);
}

static int f_check_reserved(sqlite3_file *f, int *out) {
	return REAL(f)->pMethods->xCheckReservedLock(REAL(f), out);
}

static int f_control(sqlite3_file *f, int op, void *arg) {
	return REAL(f)->pMethods->xFileControl(REAL(f), op, arg);
}

static int f_sector(sqlite3_file *f) {
	return REAL(f)->pMethods->xSectorSize(REAL(f));
}

static int f_device(sqlite3_file *f) {
	return REAL(f)->pMethods->xDeviceCharacteristics(REAL(f));
}

static int f_shm_map(sqlite3_file *f, int i, int sz, int ext, void volatile **pp) {
	return REAL(f)->pMethods->xShmMap(REAL(f), i, sz, ext, pp);
}

static int f_shm_lock(sqlite3_file *f, int off, int n, int flags) {
	return REAL(f)->pMethods->xShmLock(REAL(f), off, n, flags);
}

static void f_shm_barrier(sqlite3_file *f) {
	REAL(f)->pMethods->xShmBarrier(REAL(f));
}

static int f_shm_unmap(sqlite3_file *f, int del) {
	return REAL(f)->pMethods->xShmUnmap(REAL(f), del);
}

static int f_fetch(sqlite3_file *f, sqlite3_int64 off, int n, void **pp) {
	return REAL(f)->pMethods->xFetch(REAL(f), off, n, pp);
}

static int f_unfetch(sqlite3_file *f, sqlite3_int64 off, void *p) {
	return REAL(f)->pMethods->xUnfetch(REAL(f), off, p);
}

static const sqlite3_io_methods faulty_methods = {
	.iVersion = 3,
	.xClose = f_close,
	.xRead = f_read,
	.xWrite = f_write,
	.xTruncate = f_truncate,
	.xSync = f_sync,
	.xFileSize = f_size,
	.xLock = f_lock,
	.xUnlock = f_unlock,
	.xCheckReservedLock = f_check_reserved,
	.xFileControl = f_control,
	.xSectorSize = f_sector,
	.xDeviceCharacteristics = f_device,
	.xShmMap = f_shm_map,
	.xShmLock = f_shm_lock,
	.xShmBarrier = f_shm_barrier,
	.xShmUnmap = f_shm_unmap,
	.xFetch = f_fetch,
	.xUnfetch = f_unfetch,
};

static int v_open(sqlite3_vfs *vfs, sqlite3_filename name, sqlite3_file *f, int flags,
		  int *out_flags) {
	(void)vfs;
	struct faulty_file *ff = (struct faulty_file *)f;
	ff->real = xcalloc(1, (size_t)real_vfs->szOsFile);
	int rc = real_vfs->xOpen(real_vfs, name, ff->real, flags, out_flags);
	if (rc != SQLITE_OK) {
		xfree(ff->real);
		ff->base.pMethods = nullptr;
		return rc;
	}
	ff->base.pMethods = &faulty_methods;
	return SQLITE_OK;
}

static int v_delete(sqlite3_vfs *v, const char *n, int sync) {
	(void)v;
	return real_vfs->xDelete(real_vfs, n, sync);
}

static int v_access(sqlite3_vfs *v, const char *n, int flags, int *out) {
	(void)v;
	return real_vfs->xAccess(real_vfs, n, flags, out);
}

static int v_full(sqlite3_vfs *v, const char *n, int len, char *out) {
	(void)v;
	return real_vfs->xFullPathname(real_vfs, n, len, out);
}

static int v_random(sqlite3_vfs *v, int n, char *out) {
	(void)v;
	return real_vfs->xRandomness(real_vfs, n, out);
}

static int v_sleep(sqlite3_vfs *v, int us) {
	(void)v;
	return real_vfs->xSleep(real_vfs, us);
}

static int v_time(sqlite3_vfs *v, double *out) {
	(void)v;
	return real_vfs->xCurrentTime(real_vfs, out);
}

static int v_last_error(sqlite3_vfs *v, int n, char *out) {
	(void)v;
	return real_vfs->xGetLastError(real_vfs, n, out);
}

static int v_time64(sqlite3_vfs *v, sqlite3_int64 *out) {
	(void)v;
	return real_vfs->xCurrentTimeInt64(real_vfs, out);
}

static sqlite3_vfs faulty_vfs;

static void register_vfs(void) {
	if (real_vfs != nullptr) {
		return;
	}
	real_vfs = sqlite3_vfs_find(nullptr);
	faulty_vfs = (sqlite3_vfs){
		.iVersion = 2,
		.szOsFile = sizeof(struct faulty_file),
		.mxPathname = real_vfs->mxPathname,
		.zName = "faulty",
		.xOpen = v_open,
		.xDelete = v_delete,
		.xAccess = v_access,
		.xFullPathname = v_full,
		.xRandomness = v_random,
		.xSleep = v_sleep,
		.xCurrentTime = v_time,
		.xGetLastError = v_last_error,
		.xCurrentTimeInt64 = v_time64,
	};
	assert_int_equal(sqlite3_vfs_register(&faulty_vfs, 0), SQLITE_OK);
}

struct env {
	char dir[64];
	char uri[160];
	struct db *db;
	int64_t user, network;
};

static void env_open(struct env *e) {
	register_vfs();
	snprintf(e->dir, sizeof(e->dir), "/tmp/soju-c-faults-XXXXXX");
	assert_non_null(mkdtemp(e->dir));
	snprintf(e->uri, sizeof(e->uri), "file:%s/main.db?vfs=faulty", e->dir);
	countdown = 0;
	assert_true(db_open(e->uri, &e->db, nullptr));
	struct db_user u;
	db_user_init(&u, "alice");
	assert_true(db_store_user(e->db, &u, nullptr));
	e->user = u.id;
	db_user_free(&u);
	struct db_network n;
	db_network_init(&n, "irc.example.org");
	assert_true(db_store_network(e->db, e->user, &n, nullptr));
	e->network = n.id;
	db_network_free(&n);
}

static void env_close(struct env *e) {
	countdown = 0;
	db_close(e->db);
	const char *files[] = {"main.db", "main.db-wal", "main.db-shm"};
	for (size_t i = 0; i < 3; i++) {
		char p[128];
		snprintf(p, sizeof(p), "%s/%s", e->dir, files[i]);
		unlink(p);
	}
	rmdir(e->dir);
}

// One operation under fault k; returns whether it succeeded
typedef bool (*op_fn)(struct env *e, struct err **err);

static bool op_store_messages(struct env *e, struct err **err) {
	struct irc_msg *m[3];
	for (int i = 0; i < 3; i++) {
		m[i] = IRC_MSG("PRIVMSG", "#a", "hello");
		irc_tags_set(&m[i]->tags, "time", "2024-01-01T00:00:00.000Z");
	}
	i64vec ids = {0};
	bool ok = db_store_messages(e->db, e->network, "#a", m, 3, &ids, err);
	// All or nothing
	assert_int_equal(ids.len, ok ? 3 : 0);
	vec_free(&ids);
	for (int i = 0; i < 3; i++) {
		irc_msg_free(m[i]);
	}
	return ok;
}

static bool op_delete_network(struct env *e, struct err **err) {
	return db_delete_network(e->db, e->network, err);
}

static bool op_receipts(struct env *e, struct err **err) {
	struct db_delivery_receipt r[2];
	for (int i = 0; i < 2; i++) {
		db_delivery_receipt_init(&r[i]);
		xfree(r[i].target);
		r[i].target = xstrdup(i == 0 ? "#a" : "#b");
		xfree(r[i].internal_msg_id);
		r[i].internal_msg_id = xstrdup("id");
	}
	bool ok = db_store_client_delivery_receipts(e->db, e->network, "", r, 2, err);
	db_delivery_receipt_free(&r[0]);
	db_delivery_receipt_free(&r[1]);
	return ok;
}

static void every_fault(op_fn op, int code) {
	fault_code = code;
	int failures = 0;
	for (int k = 1;; k++) {
		struct env e;
		env_open(&e);
		faults = 0;
		countdown = k;
		struct err *err = nullptr;
		bool ok = op(&e, &err);
		countdown = 0;
		bool injected = faults > 0;
		if (!ok) {
			assert_non_null(err);
			err_free(err);
			failures++;
		}
		// The database is still usable afterwards
		struct db_stats st;
		struct err *e2 = nullptr;
		if (!db_stats(e.db, &st, &e2)) {
			fail_msg("database unusable after fault %d: %s", k, err_msg(e2));
		}
		env_close(&e);
		if (!injected) {
			// k is past the operation's last write: done
			assert_true(ok);
			break;
		}
	}
	assert_true(failures > 0);
}

static void test_store_messages_faults(void **state) {
	(void)state;
	every_fault(op_store_messages, SQLITE_IOERR_WRITE);
	every_fault(op_store_messages, SQLITE_FULL);
}

static void test_delete_network_faults(void **state) {
	(void)state;
	every_fault(op_delete_network, SQLITE_IOERR_WRITE);
	every_fault(op_delete_network, SQLITE_FULL);
}

static void test_receipts_faults(void **state) {
	(void)state;
	every_fault(op_receipts, SQLITE_IOERR_WRITE);
}

T_MAIN(cmocka_unit_test(test_store_messages_faults), cmocka_unit_test(test_delete_network_faults),
       cmocka_unit_test(test_receipts_faults))
