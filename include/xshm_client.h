#ifndef XSHM_CLIENT_H
#define XSHM_CLIENT_H

#include "xshm.h"

#ifdef __cplusplus
extern "C" {
#endif

static inline shm_endpoint_config_t xshm_client_config(const char *name) {
    shm_endpoint_config_t cfg = {name, 0u};
    return cfg;
}

static inline shm_callbacks_t xshm_client_callbacks_default(void) {
    shm_callbacks_t cb;
    cb.on_connect = 0;
    cb.on_disconnect = 0;
    cb.on_data_available = 0;
    cb.on_space_available = 0;
    cb.on_error = 0;
    cb.user_data = 0;
    cb.on_message = 0;
    cb.on_overflow = 0;
    return cb;
}

static inline shm_auto_options_t xshm_client_auto_options_default(void) {
    return shm_auto_options_default();
}

static inline shm_auto_options_t shm_client_auto_options_default(void) {
    return shm_auto_options_default();
}

#ifdef __cplusplus
}
#endif

#endif /* XSHM_CLIENT_H */

