// Declarations shared by config/config.c and config/config_directives.c
#pragma once
#include "config/config.h"
#include "config/scfg.h"

// Parses the directives of a configuration file into srv
[[nodiscard]] bool config_parse_block(const struct scfg_block *block, struct config *srv,
				      struct err **err);

typedef VEC(const struct scfg_directive *) dirvec;

void config_free_auth(struct config_auth *a);
void config_turso_free(struct config_turso *t);
void cfg_push_loopback(struct ipset *s);
void cfg_get(const struct scfg_block *b, const char *name, dirvec *out);
[[gnu::format(printf, 3, 4)]] bool cfg_dir_err(struct err **err, const struct scfg_directive *d,
					       const char *fmt, ...);
bool cfg_single(const dirvec *dirs, const struct scfg_directive **out, struct err **err);
bool cfg_single_string(const dirvec *dirs, const char **out, struct err **err);
bool cfg_single_list(const dirvec *dirs, const strvec **out, struct err **err);
bool cfg_single_pair(const dirvec *dirs, const char **a, const char **b, struct err **err);
bool cfg_parse_bool(const char *s, bool *out, struct err **err);
bool cfg_parse_driver_source(const char *name, const strvec *params, const char **driver,
			     const char **source, struct err **err);
void cfg_replace(char **field, const char *value);
bool cfg_wrapped(struct err **err, struct err *e, const char *prefix);
