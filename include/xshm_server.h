#ifndef XSHM_SERVER_H
#define XSHM_SERVER_H

#include "xshm.h"

#ifdef __cplusplus
extern "C" {
#endif

// ============================================================================
// Single-client server helpers
// ============================================================================

static inline shm_endpoint_config_t xshm_server_config(const char *name) {
    shm_endpoint_config_t cfg = {name, 0u};
    return cfg;
}

static inline shm_callbacks_t xshm_server_callbacks_default(void) {
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

static inline shm_auto_options_t xshm_server_auto_options_default(void) {
    return shm_auto_options_default();
}

static inline shm_auto_options_t shm_server_auto_options_default(void) {
    return shm_auto_options_default();
}

// ============================================================================
// Multi-client server API
// ============================================================================

// Note: Types shm_multi_options_t, shm_multi_callbacks_t, MultiServerHandle
// and all shm_multi_server_* functions are declared in xshm.h (auto-generated)

// ============================================================================
// Multi-client server helpers
// ============================================================================

static inline shm_multi_options_t xshm_multi_options_default(void) {
    return shm_multi_options_default();
}

static inline shm_multi_callbacks_t xshm_multi_callbacks_default(void) {
    return shm_multi_callbacks_default();
}

// ============================================================================
// Dispatch server helpers
// ============================================================================

// Note: Types shm_dispatch_options_t, shm_dispatch_callbacks_t,
// DispatchServerHandle and all shm_dispatch_server_* functions are declared in xshm.h

static inline shm_dispatch_callbacks_t xshm_dispatch_callbacks_default(void) {
    shm_dispatch_callbacks_t cb;
    cb.on_client_connect = 0;
    cb.on_client_disconnect = 0;
    cb.on_message = 0;
    cb.on_error = 0;
    cb.user_data = 0;
    return cb;
}

#ifdef __cplusplus
}  // extern "C"
#endif

#endif /* XSHM_SERVER_H */
