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

typedef int32_t NTSTATUS;

typedef uint32_t ULONG;

typedef uint32_t ACCESS_MASK;

typedef void *HANDLE;

#define STATUS_SUCCESS 0

#define STATUS_TIMEOUT 258

#define STATUS_WAIT_0 0

#define OBJ_CASE_INSENSITIVE 64

#define SECTION_ALL_ACCESS 983071

#define PAGE_READWRITE 4

#define SEC_COMMIT 134217728

/**
 * ViewUnmap - секция будет размаппена при закрытии handle
 */
#define VIEW_UNMAP 2

#define EVENT_ALL_ACCESS 2031619

/**
 * SynchronizationEvent - auto-reset event
 */
#define SYNCHRONIZATION_EVENT 1

/**
 * NotificationEvent - manual-reset event
 */
#define NOTIFICATION_EVENT 0

/**
 * WaitAny - вернуться когда любой объект сигнализирован
 */
#define WAIT_ANY 1

/**
 * WaitAll - вернуться когда все объекты сигнализированы
 */
#define WAIT_ALL 0

/**
 * Псевдо-handle текущего процесса
 */
#define NT_CURRENT_PROCESS (HANDLE)-1

/**
 * SECURITY_DESCRIPTOR_REVISION
 */
#define SECURITY_DESCRIPTOR_REVISION 1

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

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* XSHM_H */
