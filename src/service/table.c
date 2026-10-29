#include "service/internal.h"

static const struct service_command network_commands[] = {
	{"create",
	 "-addr <addr> [-name name] [-username username] [-pass pass] [-realname realname] "
	 "[-certfp fingerprint] [-nick nick] [-auto-away auto-away] [-enabled enabled] "
	 "[-ignore-limit] [-connect-command command]...",
	 "add a new network", false, false, nullptr, 0, service_network_create},
	{"status", "", "show a list of saved networks and their current status", false, false,
	 nullptr, 0, service_network_status},
	{"info", "[name]", "show the configuration of a network", false, false, nullptr, 0,
	 service_network_info},
	{"update",
	 "[name] [-addr addr] [-name name] [-username username] [-pass pass] [-realname realname] "
	 "[-certfp fingerprint] [-nick nick] [-auto-away auto-away] [-enabled enabled] "
	 "[-ignore-limit] [-connect-command command]...",
	 "update a network", false, false, nullptr, 0, service_network_update},
	{"delete", "<name>", "delete a network", false, false, nullptr, 0, service_network_delete},
	{"quote", "[name] <command>", "send a raw line to a network", false, false, nullptr, 0,
	 service_network_quote},
};
static const struct service_command certfp_commands[] = {
	{"generate", "[-key-type rsa|ecdsa|ed25519] [-bits N] [-network name]",
	 "generate a new self-signed certificate, defaults to using RSA-3072 key", false, false,
	 nullptr, 0, service_certfp_generate},
	{"fingerprint", "[-network name]", "show fingerprints of certificate", false, false,
	 nullptr, 0, service_certfp_fingerprints},
};
static const struct service_command sasl_commands[] = {
	{"status", "[-network name]", "show SASL status", false, false, nullptr, 0,
	 service_sasl_status},
	{"set-plain", "[-network name] <username> <password>", "set SASL PLAIN credentials", false,
	 false, nullptr, 0, service_sasl_set_plain},
	{"reset", "[-network name]", "disable SASL authentication and remove stored credentials",
	 false, false, nullptr, 0, service_sasl_reset},
};
static const struct service_command user_commands[] = {
	{"status", "[username]", "show a list of users and their current status", true, true,
	 nullptr, 0, service_user_status},
	{"create",
	 "-username <username> -password <password> [-disable-password] [-admin] [-nick <nick>] "
	 "[-realname <realname>] [-enabled=false] [-max-networks <max-networks>]",
	 "create a new soju user", true, true, nullptr, 0, service_user_create},
	{"update",
	 "[username] [-password <password>] [-disable-password] [-admin true|false] [-nick <nick>] "
	 "[-realname <realname>] [-enabled true|false] [-max-networks <max-networks>]",
	 "update a user", false, true, nullptr, 0, service_user_update},
	{"delete", "<username> [confirmation token]", "delete a user", false, true, nullptr, 0,
	 service_user_delete},
	{"run", "<username> <command>", "run a command as another user", true, true, nullptr, 0,
	 service_user_run},
};
static const struct service_command channel_commands[] = {
	{"status", "[-network name]", "show a list of saved channels and their current status",
	 false, false, nullptr, 0, service_channel_status},
	{"create",
	 "<name> [-detached true|false] [-relay-detached default|none|highlight|message] "
	 "[-reattach-on default|none|highlight|message] [-detach-after <duration>] [-detach-on "
	 "default|none|highlight|message]",
	 "create a channel", false, false, nullptr, 0, service_channel_create},
	{"update",
	 "<name> [-detached true|false] [-relay-detached default|none|highlight|message] "
	 "[-reattach-on default|none|highlight|message] [-detach-after <duration>] [-detach-on "
	 "default|none|highlight|message]",
	 "update a channel", false, false, nullptr, 0, service_channel_update},
	{"delete", "<name>", "delete a channel", false, false, nullptr, 0, service_channel_delete},
};
static const struct service_command device_certificate_commands[] = {
	{"status", "", "show a list of device certificates", false, false, nullptr, 0,
	 service_device_cert_status},
	{"create", "-fingerprint <fingerprint> -label <label>", "create a new device certificate",
	 false, false, nullptr, 0, service_device_cert_create},
	{"delete", "<fingerprint>", "delete a device certificate", false, false, nullptr, 0,
	 service_device_cert_delete},
};
static const struct service_command server_commands[] = {
	{"status", "", "show server statistics", true, true, nullptr, 0, service_server_status},
	{"notice", "<notice>", "broadcast a notice to all connected bouncer users", true, true,
	 nullptr, 0, service_server_notice},
	{"debug", "true|false",
	 "enable/disable debug logging to stderr (will leak sensitive information)", true, true,
	 nullptr, 0, service_server_debug},
};
const struct service_command service_commands[] = {
	{"help", "[command]", "print help message", false, true, nullptr, 0, service_handle_help},
	{"network", "", "", false, false, network_commands, 6, nullptr},
	{"certfp", "", "", false, false, certfp_commands, 2, nullptr},
	{"sasl", "", "", false, false, sasl_commands, 3, nullptr},
	{"user", "", "", false, true, user_commands, 5, nullptr},
	{"channel", "", "", false, false, channel_commands, 4, nullptr},
	{"device-certificate", "", "", false, false, device_certificate_commands, 3, nullptr},
	{"server", "", "", true, true, server_commands, 3, nullptr},
};
const size_t service_commands_len = sizeof(service_commands) / sizeof(service_commands[0]);
