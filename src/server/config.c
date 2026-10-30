// Load a complete configuration snapshot before publishing it to the loops.
#include "server/server.h"

#include <errno.h>
#include <stdio.h>
#include <sys/stat.h>

#include "auth/auth.h"
#include "base/alloc.h"
#include "base/utf8.h"
#include "net/url.h"

struct config_load_job {
	struct server_config *config;
	server_config_done_fn done;
	void *data;
};

static bool load_motd(const char *path, char **out, struct err **err) {
	if (path[0] == '\0') {
		return true;
	}
	FILE *file = fopen(path, "rb");
	if (file == nullptr) {
		return err_set(err, errno, "failed to load MOTD: %s", strerror(errno));
	}
	AUTO_STRBUF struct strbuf bytes = STRBUF_INIT;
	char block[4096];
	for (;;) {
		size_t length = fread(block, 1, sizeof(block), file);
		strbuf_append(&bytes, block, length);
		if (length < sizeof(block)) {
			break;
		}
	}
	bool ok = ferror(file) == 0;
	fclose(file);
	if (!ok) {
		return err_set(err, 0, "failed to read MOTD");
	}
	if (bytes.len > 0 && bytes.buf[bytes.len - 1] == '\n') {
		strbuf_truncate(&bytes, bytes.len - 1);
	}
	AUTO_STRBUF struct strbuf text = STRBUF_INIT;
	utf8_lossy(&text, strbuf_str(&bytes));
	xfree(*out);
	*out = strbuf_take(&text);
	return true;
}

static void auth_loaded(void *data, struct auth_settings *settings, const struct err *error) {
	struct config_load_job *job = data;
	struct server_config *config = job->config;
	if (error == nullptr) {
		config->plain_auth = settings->plain;
		xfree(config->http_auth_url);
		config->http_auth_url = settings->http_url;
		settings->http_url = nullptr;
		config->oauth2 = settings->oauth2;
		settings->oauth2 = nullptr;
		auth_settings_free(settings);
		job->done(job->data, config, nullptr);
	} else {
		server_config_unref(config);
		job->done(job->data, nullptr, error);
	}
	xfree(job);
}

void server_config_load(uv_loop_t *loop, const char *path, server_config_done_fn done, void *data) {
	struct server_config *config = server_config_default();
	struct config raw;
	struct err *error = nullptr;
	if (path[0] != '\0') {
		if (!config_load(path, &raw, &error)) {
			goto fail;
		}
	} else {
		config_defaults(&raw);
	}
	config_free(&config->cfg);
	config->cfg = raw;
	if (!load_motd(raw.motd_path, &config->motd, &error)) {
		goto fail;
	}
	if (strstr(raw.icon, "://") != nullptr) {
		struct url icon;
		if (!url_parse(str_from(raw.icon), &icon, &error)) {
			goto fail;
		}
		bool https = cstr_eq(icon.scheme, "https");
		url_free(&icon);
		if (!https) {
			bool failed = err_set(&error, 0, "unsupported icon URL scheme");
			(void)failed;
			goto fail;
		}
		xfree(config->icon_url);
		config->icon_url = xstrdup(raw.icon);
	} else if (raw.icon[0] != '\0') {
		struct stat st;
		if (stat(raw.icon, &st) < 0) {
			bool failed =
				err_set(&error, errno, "failed to load icon: %s", strerror(errno));
			(void)failed;
			goto fail;
		}
		xfree(config->icon_path);
		config->icon_path = xstrdup(raw.icon);
	}
	struct config_load_job *job = xmalloc(sizeof(*job));
	*job = (struct config_load_job){.config = config, .done = done, .data = data};
	auth_settings_load(loop, &config->cfg, auth_loaded, job);
	return;
fail:
	server_config_unref(config);
	done(data, nullptr, error);
	err_free(error);
}
