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
#define MAX_MESSAGES 500

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
 * Индекс в reserved[] для передачи slot_id при multi-client handshake.
 */
#define RESERVED_SLOT_ID_INDEX 0

/**
 * Специальное значение: нет свободных слотов.
 */
#define SLOT_ID_NO_SLOT 4294967295

/**
 * Response status: success.
 */
#define STATUS_OK 0

/**
 * Response status: server rejected the connection.
 */
#define STATUS_REJECTED 1

/**
 * Максимальное количество клиентов по умолчанию
 */
#define DEFAULT_MAX_CLIENTS 20

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
  SHM_ERROR_NO_SLOT = -11,
} shm_error_t;

typedef enum shm_direction_t {
  SHM_DIR_SERVER_TO_CLIENT = 0,
  SHM_DIR_CLIENT_TO_SERVER = 1,
} shm_direction_t;

/**
 * Raw handles событий для передачи в kernel driver
 *
 * Handles представлены как `isize` для совместимости с Windows HANDLE типом.
 * Эти handles можно передать в драйвер через IOCTL для event-driven IPC.
 */
typedef struct EventHandles EventHandles;

typedef void DispatchServerHandle;

/**
 * Server-side callbacks.
 */
typedef struct shm_dispatch_callbacks_t {
  void (*on_client_connect)(uint32_t client_id,
                            uint32_t pid,
                            uint16_t revision,
                            const char *name,
                            void *user_data);
  void (*on_client_disconnect)(uint32_t client_id, void *user_data);
  void (*on_message)(uint32_t client_id, const void *data, uint32_t size, void *user_data);
  void (*on_error)(int32_t client_id, enum shm_error_t error, void *user_data);
  void *user_data;
} shm_dispatch_callbacks_t;

/**
 * Server options.
 */
typedef struct shm_dispatch_options_t {
  uint32_t lobby_timeout_ms;
  uint32_t channel_connect_timeout_ms;
  uint32_t poll_timeout_ms;
  uint32_t recv_batch;
} shm_dispatch_options_t;

typedef void DispatchClientHandle;

/**
 * Registration info passed from C client.
 */
typedef struct shm_dispatch_registration_t {
  uint32_t pid;
  uint16_t revision;
  const char *name;
} shm_dispatch_registration_t;

/**
 * Client-side callbacks.
 */
typedef struct shm_dispatch_client_callbacks_t {
  void (*on_connect)(uint32_t client_id, const char *channel_name, void *user_data);
  void (*on_disconnect)(void *user_data);
  void (*on_message)(const void *data, uint32_t size, void *user_data);
  void (*on_error)(enum shm_error_t error, void *user_data);
  void *user_data;
} shm_dispatch_client_callbacks_t;

/**
 * Client options.
 */
typedef struct shm_dispatch_client_options_t {
  uint32_t lobby_timeout_ms;
  uint32_t response_timeout_ms;
  uint32_t channel_timeout_ms;
  uint32_t poll_timeout_ms;
  uint32_t recv_batch;
  uint32_t max_send_queue;
} shm_dispatch_client_options_t;

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
   * Максимальное количество клиентов (по умолчанию 20)
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

/**
 * Опции для мультиклиента
 */
typedef struct shm_multi_client_options_t {
  /**
   * Таймаут подключения к lobby в мс (по умолчанию 5000)
   */
  uint32_t lobby_timeout_ms;
  /**
   * Таймаут подключения к слоту в мс (по умолчанию 5000)
   */
  uint32_t slot_timeout_ms;
  /**
   * Таймаут ожидания событий в мс (по умолчанию 50)
   */
  uint32_t poll_timeout_ms;
  /**
   * Количество сообщений за один цикл (по умолчанию 32)
   */
  uint32_t recv_batch;
} shm_multi_client_options_t;

/**
 * Callbacks для мультиклиента
 */
typedef struct shm_multi_client_callbacks_t {
  /**
   * Вызывается при успешном подключении (slot_id — назначенный слот)
   */
  void (*on_connect)(uint32_t slot_id, void *user_data);
  /**
   * Вызывается при отключении
   */
  void (*on_disconnect)(void *user_data);
  /**
   * Вызывается при получении сообщения от сервера
   */
  void (*on_message)(const void *data, uint32_t size, void *user_data);
  /**
   * Вызывается при ошибке
   */
  void (*on_error)(enum shm_error_t error, void *user_data);
  /**
   * Пользовательские данные
   */
  void *user_data;
} shm_multi_client_callbacks_t;

/**
 * Handle мультиклиента
 */
typedef void MultiClientHandle;

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * # Safety
 * All pointers must be valid or null where documented. `name` must be a valid C string.
 */
DispatchServerHandle *shm_dispatch_server_start(const char *name,
                                                const struct shm_dispatch_callbacks_t *callbacks,
                                                const struct shm_dispatch_options_t *options);

/**
 * # Safety
 * `handle` must be a valid DispatchServerHandle. `data` must point to `size` bytes.
 */
enum shm_error_t shm_dispatch_server_send_to(DispatchServerHandle *handle,
                                             uint32_t client_id,
                                             const void *data,
                                             uint32_t size);

/**
 * # Safety
 * `handle` must be valid. `data` must point to `size` bytes. `sent_count` may be null.
 */
enum shm_error_t shm_dispatch_server_broadcast(DispatchServerHandle *handle,
                                               const void *data,
                                               uint32_t size,
                                               uint32_t *sent_count);

/**
 * # Safety
 * `handle` must be a valid DispatchServerHandle or null.
 */
uint32_t shm_dispatch_server_client_count(const DispatchServerHandle *handle);

/**
 * # Safety
 * `handle` must be a valid DispatchServerHandle or null. Consumes the handle.
 */
void shm_dispatch_server_stop(DispatchServerHandle *handle);

/**
 * # Safety
 * All pointers must be valid. `name` and `reg.name` must be valid C strings.
 */
DispatchClientHandle *shm_dispatch_client_connect(const char *name,
                                                  const struct shm_dispatch_registration_t *reg,
                                                  const struct shm_dispatch_client_callbacks_t *callbacks,
                                                  const struct shm_dispatch_client_options_t *options);

/**
 * # Safety
 * `handle` must be a valid DispatchClientHandle. `data` must point to `size` bytes.
 */
enum shm_error_t shm_dispatch_client_send(DispatchClientHandle *handle,
                                          const void *data,
                                          uint32_t size);

/**
 * # Safety
 * `handle` must be a valid DispatchClientHandle or null. Consumes the handle.
 */
void shm_dispatch_client_stop(DispatchClientHandle *handle);

struct shm_dispatch_options_t shm_dispatch_options_default(void);

struct shm_dispatch_client_options_t shm_dispatch_client_options_default(void);

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
 * Получить event handles для передачи в kernel driver
 *
 * Возвращает структуру с raw handles (isize) для event-driven IPC.
 * Для anonymous серверов (без событий) возвращает handles с нулевыми значениями.
 *
 * # Parameters
 * - `handle`: Handle сервера
 * - `out`: Указатель на структуру для записи handles (может быть NULL)
 *
 * # Returns
 * true если handles успешно получены, false при ошибке
 */
bool shm_server_get_event_handles(ServerHandle *handle,
                                  struct EventHandles *out);

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

/**
 * Получить опции клиента по умолчанию
 */
struct shm_multi_client_options_t shm_multi_client_options_default(void);

/**
 * Получить callbacks клиента по умолчанию (все NULL)
 */
struct shm_multi_client_callbacks_t shm_multi_client_callbacks_default(void);

/**
 * Подключение мультиклиента к серверу
 *
 * Клиент автоматически:
 * 1. Подключается к lobby (base_name)
 * 2. Получает назначенный slot_id от сервера
 * 3. Переподключается к слоту (base_name_N)
 *
 * # Parameters
 * - `base_name`: Базовое имя канала (то же что у MultiServer)
 * - `callbacks`: Callbacks для событий
 * - `options`: Опции клиента (NULL для значений по умолчанию)
 *
 * # Returns
 * Handle клиента или NULL при ошибке
 */
MultiClientHandle *shm_multi_client_connect(const char *base_name,
                                            const struct shm_multi_client_callbacks_t *callbacks,
                                            const struct shm_multi_client_options_t *options);

/**
 * Отправка сообщения серверу
 *
 * # Parameters
 * - `handle`: Handle клиента
 * - `data`: Данные для отправки
 * - `size`: Размер данных
 *
 * # Returns
 * SHM_SUCCESS или код ошибки
 */
enum shm_error_t shm_multi_client_send(MultiClientHandle *handle, const void *data, uint32_t size);

/**
 * Получение назначенного slot_id
 *
 * # Parameters
 * - `handle`: Handle клиента
 *
 * # Returns
 * slot_id или SLOT_ID_NO_SLOT (0xFFFFFFFF) если не подключён
 */
uint32_t shm_multi_client_slot_id(const MultiClientHandle *handle);

/**
 * Проверка подключения клиента
 *
 * # Parameters
 * - `handle`: Handle клиента
 *
 * # Returns
 * true если клиент подключён к слоту
 */
bool shm_multi_client_is_connected(const MultiClientHandle *handle);

/**
 * Отключение мультиклиента
 *
 * # Parameters
 * - `handle`: Handle клиента
 */
void shm_multi_client_disconnect(MultiClientHandle *handle);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* XSHM_H */
