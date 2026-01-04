#ifndef XSHM_H
#define XSHM_H

#pragma once

#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <stdint.h>
#include <stdbool.h>
#include <stddef.h>

/**
 * Общее «магическое» значение для сегмента.
 */
#define SHARED_MAGIC 1481853005

/**
 * Текущая версия протокола.
 */
#define SHARED_VERSION 65536

/**
 * Размер каждого кольцевого буфера (байты).
 */
#define RING_CAPACITY ((2 * 1024) * 1024)

/**
 * Маска размера (так как это степень двойки).
 */
#define RING_MASK ((uint32_t)RING_CAPACITY - 1)

/**
 * Максимальное количество сообщений в очереди.
 */
#define MAX_MESSAGES 250

/**
 * Максимальный размер одного сообщения.
 */
#define MAX_MESSAGE_SIZE 65535

/**
 * Минимальный размер сообщения.
 */
#define MIN_MESSAGE_SIZE 2

/**
 * Размер служебного заголовка сообщения (байты).
 */
#define MESSAGE_HEADER_SIZE 4

/**
 * Состояния handshake.
 */
#define HANDSHAKE_IDLE 0

#define HANDSHAKE_CLIENT_HELLO 1

#define HANDSHAKE_SERVER_READY 2

/**
 * Максимальное количество клиентов по умолчанию
 */
#define DEFAULT_MAX_CLIENTS 10

typedef enum shm_error_t {
  SHM_SUCCESS = 0,
  SHM_ERROR_INVALID_PARAM = -1,
  SHM_ERROR_MEMORY = -2,
  SHM_ERROR_TIMEOUT = -3,
  SHM_ERROR_EMPTY = -4,
  SHM_ERROR_EXISTS = -5,
  SHM_ERROR_NOT_FOUND = -6,
  SHM_ERROR_ACCESS = -7,
  SHM_ERROR_NOT_READY = -8,
  SHM_ERROR_PROTOCOL = -9,
  SHM_ERROR_FULL = -10,
} shm_error_t;

typedef enum shm_direction_t {
  SHM_DIR_SERVER_TO_CLIENT = 0,
  SHM_DIR_CLIENT_TO_SERVER = 1,
} shm_direction_t;

typedef struct shm_auto_options_t {
  uint32_t wait_timeout_ms;
  uint32_t reconnect_delay_ms;
  uint32_t connect_timeout_ms;
  uint32_t max_send_queue;
  uint32_t recv_batch;
} shm_auto_options_t;

typedef void AutoServerHandle;

typedef struct shm_endpoint_config_t {
  const char *name;
  uint32_t buffer_bytes;
} shm_endpoint_config_t;

typedef struct shm_callbacks_t {
  void (*on_connect)(void *user_data);
  void (*on_disconnect)(void *user_data);
  void (*on_data_available)(void *user_data);
  void (*on_space_available)(void *user_data);
  void (*on_error)(enum shm_error_t error, void *user_data);
  void *user_data;
  void (*on_message)(enum shm_direction_t direction,
                     const void *data,
                     uint32_t size,
                     void *user_data);
  void (*on_overflow)(enum shm_direction_t direction, uint32_t dropped, void *user_data);
} shm_callbacks_t;

typedef struct shm_auto_stats_t {
  uint64_t sent_messages;
  uint64_t send_overflows;
  uint64_t received_messages;
  uint64_t receive_overflows;
} shm_auto_stats_t;

typedef void AutoClientHandle;

typedef void ServerHandle;

typedef void ClientHandle;

/**
 * Опции для мультиклиентного сервера
 */
typedef struct shm_multi_options_t {
  /**
   * Максимальное количество клиентов (по умолчанию 10)
   */
  uint32_t max_clients;
  /**
   * Таймаут ожидания событий в мс (по умолчанию 50)
   */
  uint32_t poll_timeout_ms;
  /**
   * Количество сообщений за один цикл (по умолчанию 32)
   */
  uint32_t recv_batch;
} shm_multi_options_t;

/**
 * Callbacks для мультиклиентного сервера
 */
typedef struct shm_multi_callbacks_t {
  /**
   * Вызывается при подключении клиента
   */
  void (*on_client_connect)(uint32_t client_id, void *user_data);
  /**
   * Вызывается при отключении клиента
   */
  void (*on_client_disconnect)(uint32_t client_id, void *user_data);
  /**
   * Вызывается при получении сообщения
   */
  void (*on_message)(uint32_t client_id, const void *data, uint32_t size, void *user_data);
  /**
   * Вызывается при ошибке (client_id = u32::MAX для общих ошибок)
   */
  void (*on_error)(uint32_t client_id, enum shm_error_t error, void *user_data);
  /**
   * Пользовательские данные, передаются во все callbacks
   */
  void *user_data;
} shm_multi_callbacks_t;

/**
 * Handle мультиклиентного сервера
 */
typedef void MultiServerHandle;

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

struct shm_auto_options_t shm_auto_options_default(void);

AutoServerHandle *shm_server_start_auto(const struct shm_endpoint_config_t *config,
                                        const struct shm_callbacks_t *callbacks,
                                        const struct shm_auto_options_t *options);

enum shm_error_t shm_server_send_auto(AutoServerHandle *handle, const void *data, uint32_t size);

bool shm_server_stats_auto(const AutoServerHandle *handle, struct shm_auto_stats_t *out);

void shm_server_stop_auto(AutoServerHandle *handle);

AutoClientHandle *shm_client_connect_auto(const struct shm_endpoint_config_t *config,
                                          const struct shm_callbacks_t *callbacks,
                                          const struct shm_auto_options_t *options);

enum shm_error_t shm_client_send_auto(AutoClientHandle *handle, const void *data, uint32_t size);

bool shm_client_stats_auto(const AutoClientHandle *handle, struct shm_auto_stats_t *out);

void shm_client_disconnect_auto(AutoClientHandle *handle);

ServerHandle *shm_server_start(const struct shm_endpoint_config_t *config,
                               const struct shm_callbacks_t *callbacks);

enum shm_error_t shm_server_wait_for_client(ServerHandle *handle, uint32_t timeout_ms);

void shm_server_stop(ServerHandle *handle);

enum shm_error_t shm_server_send(ServerHandle *handle, const void *data, uint32_t size);

enum shm_error_t shm_server_receive(ServerHandle *handle, void *buffer, uint32_t *size);

enum shm_error_t shm_server_poll(ServerHandle *handle, uint32_t timeout_ms);

ClientHandle *shm_client_connect(const struct shm_endpoint_config_t *config,
                                 const struct shm_callbacks_t *callbacks,
                                 uint32_t timeout_ms);

void shm_client_disconnect(ClientHandle *handle);

bool shm_client_is_connected(const ClientHandle *handle);

enum shm_error_t shm_client_send(ClientHandle *handle, const void *data, uint32_t size);

enum shm_error_t shm_client_receive(ClientHandle *handle, void *buffer, uint32_t *size);

enum shm_error_t shm_client_poll(ClientHandle *handle, uint32_t timeout_ms);

/**
 * Получить опции по умолчанию
 */
struct shm_multi_options_t shm_multi_options_default(void);

/**
 * Получить callbacks по умолчанию (все NULL)
 */
struct shm_multi_callbacks_t shm_multi_callbacks_default(void);

/**
 * Запуск мультиклиентного сервера
 *
 * # Parameters
 * - `base_name`: Базовое имя канала (клиенты подключаются к "{base_name}_{slot_id}")
 * - `callbacks`: Callbacks для событий
 * - `options`: Опции сервера (NULL для значений по умолчанию)
 *
 * # Returns
 * Handle сервера или NULL при ошибке
 */
MultiServerHandle *shm_multi_server_start(const char *base_name,
                                          const struct shm_multi_callbacks_t *callbacks,
                                          const struct shm_multi_options_t *options);

/**
 * Отправка сообщения конкретному клиенту
 *
 * # Parameters
 * - `handle`: Handle сервера
 * - `client_id`: ID клиента
 * - `data`: Данные для отправки
 * - `size`: Размер данных
 *
 * # Returns
 * SHM_SUCCESS или код ошибки
 */
enum shm_error_t shm_multi_server_send_to(MultiServerHandle *handle,
                                          uint32_t client_id,
                                          const void *data,
                                          uint32_t size);

/**
 * Отправка сообщения всем подключённым клиентам
 *
 * # Parameters
 * - `handle`: Handle сервера
 * - `data`: Данные для отправки
 * - `size`: Размер данных
 * - `sent_count`: (out) Количество клиентов, которым отправлено (может быть NULL)
 *
 * # Returns
 * SHM_SUCCESS или код ошибки
 */
enum shm_error_t shm_multi_server_broadcast(MultiServerHandle *handle,
                                            const void *data,
                                            uint32_t size,
                                            uint32_t *sent_count);

/**
 * Отключение клиента
 *
 * # Parameters
 * - `handle`: Handle сервера
 * - `client_id`: ID клиента для отключения
 *
 * # Returns
 * SHM_SUCCESS или код ошибки
 */
enum shm_error_t shm_multi_server_disconnect_client(MultiServerHandle *handle, uint32_t client_id);

/**
 * Получение количества подключённых клиентов
 *
 * # Parameters
 * - `handle`: Handle сервера
 *
 * # Returns
 * Количество подключённых клиентов или 0 при ошибке
 */
uint32_t shm_multi_server_client_count(const MultiServerHandle *handle);

/**
 * Проверка подключения конкретного клиента
 *
 * # Parameters
 * - `handle`: Handle сервера
 * - `client_id`: ID клиента
 *
 * # Returns
 * true если клиент подключён
 */
bool shm_multi_server_is_client_connected(const MultiServerHandle *handle, uint32_t client_id);

/**
 * Получение списка подключённых клиентов
 *
 * # Parameters
 * - `handle`: Handle сервера
 * - `client_ids`: Буфер для записи ID клиентов
 * - `max_count`: Размер буфера
 * - `actual_count`: (out) Фактическое количество клиентов
 *
 * # Returns
 * SHM_SUCCESS или код ошибки
 */
enum shm_error_t shm_multi_server_get_clients(const MultiServerHandle *handle,
                                              uint32_t *client_ids,
                                              uint32_t max_count,
                                              uint32_t *actual_count);

/**
 * Получение имени канала для конкретного слота
 *
 * # Parameters
 * - `handle`: Handle сервера
 * - `slot_id`: ID слота
 * - `buffer`: Буфер для записи имени
 * - `buffer_size`: Размер буфера
 *
 * # Returns
 * Длина имени (без null-терминатора) или 0 при ошибке
 */
uint32_t shm_multi_server_channel_name(const MultiServerHandle *handle,
                                       uint32_t slot_id,
                                       char *buffer,
                                       uint32_t buffer_size);

/**
 * Остановка мультиклиентного сервера
 *
 * # Parameters
 * - `handle`: Handle сервера
 */
void shm_multi_server_stop(MultiServerHandle *handle);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* XSHM_H */
