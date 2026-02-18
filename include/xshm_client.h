#ifndef XSHM_CLIENT_H
#define XSHM_CLIENT_H

#include "xshm.h"

#ifdef __cplusplus
extern "C" {
#endif

// ============================================================================
// Single-client helpers
// ============================================================================

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

// ============================================================================
// Multi-client helpers
// ============================================================================

// Note: Types shm_multi_client_options_t, shm_multi_client_callbacks_t,
// MultiClientHandle and all shm_multi_client_* functions are declared in xshm.h

static inline shm_multi_client_options_t xshm_multi_client_options_default(void) {
    return shm_multi_client_options_default();
}

static inline shm_multi_client_callbacks_t xshm_multi_client_callbacks_default(void) {
    return shm_multi_client_callbacks_default();
}

// ============================================================================
// Dispatch client helpers
// ============================================================================

// Note: Types shm_dispatch_client_options_t, shm_dispatch_client_callbacks_t,
// shm_dispatch_registration_t, DispatchClientHandle and all shm_dispatch_client_*
// functions are declared in xshm.h

static inline shm_dispatch_client_callbacks_t xshm_dispatch_client_callbacks_default(void) {
    shm_dispatch_client_callbacks_t cb;
    cb.on_connect = 0;
    cb.on_disconnect = 0;
    cb.on_message = 0;
    cb.on_error = 0;
    cb.user_data = 0;
    return cb;
}

static inline shm_dispatch_registration_t xshm_dispatch_registration(uint32_t pid, uint16_t revision, const char *name) {
    shm_dispatch_registration_t reg;
    reg.pid = pid;
    reg.revision = revision;
    reg.name = name;
    return reg;
}

#ifdef __cplusplus
}
#endif

#endif /* XSHM_CLIENT_H */

