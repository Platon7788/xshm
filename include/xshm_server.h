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

/**
 * @brief Опции для мультиклиентного сервера
 */
typedef struct shm_multi_options_t {
    /** Максимальное количество клиентов (по умолчанию 10) */
    uint32_t max_clients;
    /** Таймаут ожидания событий в мс (по умолчанию 50) */
    uint32_t poll_timeout_ms;
    /** Количество сообщений за один цикл (по умолчанию 32) */
    uint32_t recv_batch;
} shm_multi_options_t;

/**
 * @brief Callbacks для мультиклиентного сервера
 */
typedef struct shm_multi_callbacks_t {
    /** Вызывается при подключении клиента */
    void (*on_client_connect)(uint32_t client_id, void* user_data);
    /** Вызывается при отключении клиента */
    void (*on_client_disconnect)(uint32_t client_id, void* user_data);
    /** Вызывается при получении сообщения */
    void (*on_message)(uint32_t client_id, const void* data, uint32_t size, void* user_data);
    /** Вызывается при ошибке (client_id = UINT32_MAX для общих ошибок) */
    void (*on_error)(uint32_t client_id, shm_error_t error, void* user_data);
    /** Пользовательские данные, передаются во все callbacks */
    void* user_data;
} shm_multi_callbacks_t;

/** Handle мультиклиентного сервера */
typedef void MultiServerHandle;

/** Получить опции по умолчанию */
shm_multi_options_t shm_multi_options_default(void);

/** Получить callbacks по умолчанию (все NULL) */
shm_multi_callbacks_t shm_multi_callbacks_default(void);

/**
 * @brief Запуск мультиклиентного сервера
 *
 * @param base_name Базовое имя канала. Клиенты подключаются к "{base_name}_{slot_id}"
 * @param callbacks Callbacks для событий (может быть NULL)
 * @param options Опции сервера (NULL для значений по умолчанию)
 * @return Handle сервера или NULL при ошибке
 */
MultiServerHandle* shm_multi_server_start(
    const char* base_name,
    const shm_multi_callbacks_t* callbacks,
    const shm_multi_options_t* options
);

/**
 * @brief Отправка сообщения конкретному клиенту
 */
shm_error_t shm_multi_server_send_to(
    MultiServerHandle* handle,
    uint32_t client_id,
    const void* data,
    uint32_t size
);

/**
 * @brief Отправка сообщения всем подключённым клиентам
 *
 * @param sent_count [out] Количество клиентов, которым отправлено (может быть NULL)
 */
shm_error_t shm_multi_server_broadcast(
    MultiServerHandle* handle,
    const void* data,
    uint32_t size,
    uint32_t* sent_count
);

/**
 * @brief Принудительное отключение клиента
 */
shm_error_t shm_multi_server_disconnect_client(
    MultiServerHandle* handle,
    uint32_t client_id
);

/**
 * @brief Получение количества подключённых клиентов
 */
uint32_t shm_multi_server_client_count(const MultiServerHandle* handle);

/**
 * @brief Проверка подключения конкретного клиента
 */
bool shm_multi_server_is_client_connected(
    const MultiServerHandle* handle,
    uint32_t client_id
);

/**
 * @brief Получение списка подключённых клиентов
 *
 * @param client_ids Буфер для записи ID (может быть NULL для получения только количества)
 * @param max_count Размер буфера
 * @param actual_count [out] Фактическое количество клиентов
 */
shm_error_t shm_multi_server_get_clients(
    const MultiServerHandle* handle,
    uint32_t* client_ids,
    uint32_t max_count,
    uint32_t* actual_count
);

/**
 * @brief Получение имени канала для конкретного слота
 *
 * Клиенты используют это имя для подключения через shm_client_connect_auto()
 *
 * @return Длина имени (без null-терминатора) или 0 при ошибке
 */
uint32_t shm_multi_server_channel_name(
    const MultiServerHandle* handle,
    uint32_t slot_id,
    char* buffer,
    uint32_t buffer_size
);

/**
 * @brief Остановка мультиклиентного сервера
 */
void shm_multi_server_stop(MultiServerHandle* handle);

// ============================================================================
// Multi-client server helpers
// ============================================================================

static inline shm_multi_options_t xshm_multi_options_default(void) {
    return shm_multi_options_default();
}

static inline shm_multi_callbacks_t xshm_multi_callbacks_default(void) {
    return shm_multi_callbacks_default();
}

#ifdef __cplusplus
}  // extern "C"
#endif

#endif /* XSHM_SERVER_H */
