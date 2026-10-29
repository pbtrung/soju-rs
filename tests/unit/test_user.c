#include "testsupport.h"
#include "base/alloc.h"
#include "user/user.h"

static void test_store(void **state) {
	(void)state;
	struct delivered_store s;
	delivered_init(&s, CASEMAPPING_RFC1459);
	delivered_set(&s, "#Chan", "phone", "1");
	delivered_set(&s, "#chan", "phone", "22");
	delivered_set(&s, "#chan", "laptop", "3");
	assert_int_equal(cmmap_len(&s.targets), 1);
	assert_string_equal(delivered_get(&s, "#CHAN", "phone"), "22");
	assert_string_equal(delivered_get(&s, "#chan", "laptop"), "3");
	assert_string_equal(delivered_get(&s, "#chan", "tablet"), "");
	delivered_destroy(&s);
}
static void test_ident(void **state) {
	(void)state;
	AUTOFREE char *ident = user_ident(1);
	assert_int_equal(strlen(ident), 32);
}
static void test_local_addr(void **state) {
	(void)state;
	struct ipnet nets[2];
	assert_true(ipnet_parse(STR_LIT("192.168.0.0/30"), &nets[0]));
	assert_true(ipnet_parse(STR_LIT("fd00::/64"), &nets[1]));
	struct ipaddr remote, local;
	bool found;
	assert_true(ipaddr_parse(STR_LIT("1.2.3.4"), &remote));
	assert_true(user_local_tcp_addr(nets, 2, 0, remote, &local, &found, nullptr));
	assert_true(found);
	AUTO_STRBUF struct strbuf text = STRBUF_INIT;
	ipaddr_format(&text, local);
	assert_sb_eq(&text, "192.168.0.1");
	assert_false(user_local_tcp_addr(nets, 2, 5, remote, &local, &found, nullptr));
	assert_true(ipaddr_parse(STR_LIT("::1"), &remote));
	assert_true(user_local_tcp_addr(nets, 2, 9, remote, &local, &found, nullptr));
	assert_true(found);
	strbuf_reset(&text);
	ipaddr_format(&text, local);
	assert_sb_eq(&text, "fd00::a");
	assert_true(user_local_tcp_addr(nets, 1, 0, remote, &local, &found, nullptr));
	assert_false(found);
}
static void test_highlight(void **state) {
	(void)state;
	struct user u = {0};
	struct user_network net = {0};
	db_user_init(&u.record, "alice");
	xfree(u.record.nick);
	u.record.nick = xstrdup("Alice");
	db_network_init(&net.record, "irc.example.org");
	struct irc_msg *m = IRC_MSG("PRIVMSG", "#chan", "hi Alice");
	irc_msg_set_prefix(m, "bob", "", "");
	assert_true(user_network_highlight(&u, &net, m));
	irc_msg_set_param(m, 1, "hi");
	assert_false(user_network_highlight(&u, &net, m));
	irc_msg_set_param(m, 1, "hi Alice");
	irc_msg_set_prefix(m, "ALICE", "", "");
	assert_false(user_network_highlight(&u, &net, m));
	irc_msg_free(m);
	db_network_free(&net.record);
	db_user_free(&u.record);
}
static void test_ring(void **state) {
	(void)state;
	struct msgstore s;
	msgstore_init(&s, nullptr);
	char *zero;
	assert_true(msgstore_last_id(&s, 1, "#a", &zero, nullptr));
	for (unsigned i = 0; i < 5000; i++) {
		AUTOFREE char *text = xasprintf("%u", i);
		struct irc_msg *m = IRC_MSG("PRIVMSG", "#a", text);
		char *id;
		assert_true(msgstore_append(&s, 1, "#a", m, &id, nullptr));
		xfree(id);
		irc_msg_free(m);
	}
	irc_msg_vec history = {0};
	assert_true(msgstore_load_latest_id(&s, 1, "#a", zero, 3, false, &history, nullptr));
	assert_int_equal(history.len, 3);
	assert_string_equal(irc_msg_param(history.data[0], 1), "4997");
	assert_string_equal(irc_msg_param(history.data[2], 1), "4999");
	irc_msg_vec_free(&history);
	AUTOFREE char *next = format_memory_msg_id(1, STR_LIT("#a"), 5001);
	assert_true(msgstore_load_latest_id(&s, 1, "#a", next, 3, false, &history, nullptr));
	assert_int_equal(history.len, 0);
	AUTOFREE char *future = format_memory_msg_id(1, STR_LIT("#a"), 5002);
	assert_false(msgstore_load_latest_id(&s, 1, "#a", future, 3, false, &history, nullptr));
	xfree(zero);
	msgstore_destroy(&s);
}
static void test_delete_network(void **state) {
	(void)state;
	struct msgstore s;
	msgstore_init(&s, nullptr);
	struct irc_msg *m = IRC_MSG("PRIVMSG", "#a", "hi");
	char *id;
	assert_true(msgstore_append(&s, 1, "#a", m, &id, nullptr));
	xfree(id);
	assert_true(msgstore_append(&s, 2, "#a", m, &id, nullptr));
	xfree(id);
	msgstore_delete_network(&s, 1);
	assert_int_equal(strmap_len(&s.buffers), 1);
	irc_msg_free(m);
	msgstore_destroy(&s);
}
T_MAIN(cmocka_unit_test(test_store), cmocka_unit_test(test_ident),
       cmocka_unit_test(test_local_addr), cmocka_unit_test(test_highlight),
       cmocka_unit_test(test_ring), cmocka_unit_test(test_delete_network))
